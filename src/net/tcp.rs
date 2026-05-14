use crate::fs::fops::FileOps;
use crate::fs::open_file::FileCtx;
use crate::memory::uaccess::{copy_from_user_slice, copy_to_user_slice};
use crate::net::sops::{RecvFlags, SendFlags, SocketOps};
use crate::net::{
    ShutdownHow, SockAddr, allocate_ephemeral_port, normalize_local_endpoint_for_peer,
    poll_network, process_packets, tcp_socket_remote_endpoint, tcp_socket_state,
    wait_for_network_progress, with_net_core,
};
use crate::sync::SpinLock;
use alloc::boxed::Box;
use alloc::vec;
use alloc::vec::Vec;
use async_trait::async_trait;
use core::net::Ipv4Addr;
use core::sync::atomic::{AtomicUsize, Ordering};
use libkernel::error::KernelError;
use libkernel::memory::address::UA;
use smoltcp::iface::SocketHandle;
use smoltcp::socket::tcp::{self as smol_tcp, SocketBuffer};
use smoltcp::wire::{IpAddress, IpEndpoint, IpListenEndpoint};

const BACKLOG_MAX: usize = 8;

pub struct TcpSocket {
    handle: SocketHandle,
    local_endpoint: SpinLock<Option<IpListenEndpoint>>,
    backlogs: SpinLock<Vec<TcpSocket>>,
    num_backlogs: AtomicUsize,
    rd_shutdown: SpinLock<bool>,
    wr_shutdown: SpinLock<bool>,
}

impl TcpSocket {
    pub fn new() -> Result<Self, KernelError> {
        let rx_buffer = SocketBuffer::new(vec![0; 4096]);
        let tx_buffer = SocketBuffer::new(vec![0; 4096]);
        let inner = smol_tcp::Socket::new(rx_buffer, tx_buffer);
        let handle = with_net_core(|core| core.sockets.add(inner))?;
        Ok(TcpSocket {
            handle,
            local_endpoint: SpinLock::new(None),
            backlogs: SpinLock::new(Vec::new()),
            num_backlogs: AtomicUsize::new(0),
            rd_shutdown: SpinLock::new(false),
            wr_shutdown: SpinLock::new(false),
        })
    }

    fn destroy_handle(&self) {
        let _ = with_net_core(|core| {
            {
                let socket = core.sockets.get_mut::<smol_tcp::Socket>(self.handle);
                socket.abort();
            }
            let _ = core.sockets.remove(self.handle);
        });
    }

    fn normalized_local_endpoint(&self) -> IpListenEndpoint {
        let mut guard = self.local_endpoint.lock_save_irq();
        let mut endpoint = guard.unwrap_or(IpListenEndpoint {
            addr: None,
            port: 0,
        });
        if endpoint.port == 0 {
            endpoint.port = allocate_ephemeral_port();
        }
        *guard = Some(endpoint);
        endpoint
    }

    fn refill_backlog_sockets(&self, backlogs: &mut Vec<TcpSocket>) -> Result<(), KernelError> {
        let target = self.num_backlogs.load(Ordering::Relaxed);
        let local_endpoint = self.normalized_local_endpoint();

        while backlogs.len() < target {
            let socket = TcpSocket::new()?;
            with_net_core(|core| {
                core.sockets
                    .get_mut::<smol_tcp::Socket>(socket.handle)
                    .listen(local_endpoint)
                    .map_err(|_| KernelError::InvalidValue)
            })??;
            backlogs.push(socket);
        }

        Ok(())
    }

    fn remote_addr(&self) -> Option<SockAddr> {
        tcp_socket_remote_endpoint(self.handle)
            .ok()
            .flatten()
            .map(SockAddr::from)
    }

    async fn wait_until<F, R>(&self, nonblock: bool, mut f: F) -> Result<R, KernelError>
    where
        F: FnMut() -> Result<Option<R>, KernelError>,
    {
        loop {
            poll_network()?;
            if let Some(result) = f()? {
                return Ok(result);
            }
            if nonblock {
                return Err(KernelError::TryAgain);
            }
            wait_for_network_progress().await;
        }
    }
}

#[async_trait]
impl SocketOps for TcpSocket {
    async fn bind(&self, addr: SockAddr) -> Result<(), KernelError> {
        let mut endpoint: IpListenEndpoint = addr.try_into()?;
        if endpoint.port == 0 {
            endpoint.port = allocate_ephemeral_port();
        }
        *self.local_endpoint.lock_save_irq() = Some(endpoint);
        Ok(())
    }

    async fn connect(&self, addr: SockAddr) -> Result<(), KernelError> {
        let remote_endpoint: IpEndpoint = addr.try_into()?;
        let mut local_endpoint = self.normalized_local_endpoint();
        normalize_local_endpoint_for_peer(&mut local_endpoint, remote_endpoint);
        *self.local_endpoint.lock_save_irq() = Some(local_endpoint);

        with_net_core(|core| {
            core.sockets
                .get_mut::<smol_tcp::Socket>(self.handle)
                .connect(core.iface.context(), remote_endpoint, local_endpoint)
                .map_err(|_| KernelError::InvalidValue)
        })??;

        self.wait_until(false, || {
            let state = tcp_socket_state(self.handle)?;
            match state {
                smol_tcp::State::Established
                | smol_tcp::State::CloseWait
                | smol_tcp::State::FinWait1
                | smol_tcp::State::FinWait2 => Ok(Some(())),
                smol_tcp::State::Closed | smol_tcp::State::TimeWait => Err(KernelError::TimedOut),
                _ => Ok(None),
            }
        })
        .await
    }

    async fn listen(&self, backlog: i32) -> Result<(), KernelError> {
        let target = backlog.max(1) as usize;
        let target = target.min(BACKLOG_MAX);
        self.num_backlogs.store(target, Ordering::Relaxed);

        let mut backlogs = self.backlogs.lock_save_irq();
        while backlogs.len() > target {
            if let Some(sock) = backlogs.pop() {
                sock.destroy_handle();
            }
        }
        self.refill_backlog_sockets(&mut backlogs)
    }

    async fn accept(&self) -> Result<(Box<dyn SocketOps>, SockAddr), KernelError> {
        if self.num_backlogs.load(Ordering::Relaxed) == 0 {
            return Err(KernelError::InvalidValue);
        }

        loop {
            poll_network()?;

            let ready = {
                let backlogs = self.backlogs.lock_save_irq();
                with_net_core(|core| {
                    backlogs.iter().enumerate().find_map(|(idx, sock)| {
                        let socket = core.sockets.get::<smol_tcp::Socket>(sock.handle);
                        match socket.state() {
                            smol_tcp::State::Established
                            | smol_tcp::State::CloseWait
                            | smol_tcp::State::FinWait1
                            | smol_tcp::State::FinWait2 => {
                                Some((idx, socket.remote_endpoint().map(SockAddr::from)))
                            }
                            _ => None,
                        }
                    })
                })?
            };

            if let Some((idx, peer_addr)) = ready {
                let mut backlogs = self.backlogs.lock_save_irq();
                let sock = backlogs.remove(idx);
                self.refill_backlog_sockets(&mut backlogs)?;
                let peer_addr = peer_addr.unwrap_or_else(|| {
                    SockAddr::from(IpEndpoint {
                        addr: IpAddress::Ipv4(Ipv4Addr::UNSPECIFIED),
                        port: 0,
                    })
                });
                return Ok((Box::new(sock), peer_addr));
            }

            wait_for_network_progress().await;
        }
    }

    async fn recv(
        &mut self,
        _ctx: &mut FileCtx,
        buf: UA,
        count: usize,
        flags: RecvFlags,
    ) -> Result<(usize, Option<SockAddr>), KernelError> {
        if count == 0 || *self.rd_shutdown.lock_save_irq() {
            return Ok((0, self.remote_addr()));
        }

        let peer = self.remote_addr();
        let nonblock = flags.contains(RecvFlags::MSG_DONTWAIT);
        let data = self
            .wait_until(nonblock, || {
                with_net_core(|core| {
                    let socket = core.sockets.get_mut::<smol_tcp::Socket>(self.handle);
                    if socket.can_recv() {
                        let mut data = vec![0u8; count];
                        let len = socket
                            .recv_slice(&mut data)
                            .map_err(|_| KernelError::InvalidValue)?;
                        data.truncate(len);
                        Ok(Some(data))
                    } else if !socket.may_recv() {
                        Ok(Some(Vec::new()))
                    } else {
                        Ok(None)
                    }
                })?
            })
            .await?;

        let len = data.len();
        copy_to_user_slice(&data, buf).await?;
        Ok((len, peer))
    }

    async fn recvfrom(
        &mut self,
        ctx: &mut FileCtx,
        buf: UA,
        count: usize,
        flags: RecvFlags,
        _addr: Option<SockAddr>,
    ) -> Result<(usize, Option<SockAddr>), KernelError> {
        self.recv(ctx, buf, count, flags).await
    }

    async fn send(
        &mut self,
        _ctx: &mut FileCtx,
        buf: UA,
        count: usize,
        flags: SendFlags,
    ) -> Result<usize, KernelError> {
        if count == 0 {
            return Ok(0);
        }
        if *self.wr_shutdown.lock_save_irq() {
            return Err(KernelError::BrokenPipe);
        }

        let mut data = vec![0u8; count];
        copy_from_user_slice(buf, &mut data).await?;
        let nonblock = flags.contains(SendFlags::MSG_DONT_WAIT);

        let sent = self
            .wait_until(nonblock, || {
                with_net_core(|core| {
                    let socket = core.sockets.get_mut::<smol_tcp::Socket>(self.handle);
                    if socket.can_send() {
                        let len = socket
                            .send_slice(&data)
                            .map_err(|_| KernelError::BrokenPipe)?;
                        Ok(Some(len))
                    } else if !socket.may_send() {
                        Err(KernelError::BrokenPipe)
                    } else {
                        Ok(None)
                    }
                })?
            })
            .await?;

        process_packets();
        Ok(sent)
    }

    async fn sendto(
        &mut self,
        ctx: &mut FileCtx,
        buf: UA,
        count: usize,
        flags: SendFlags,
        _addr: SockAddr,
    ) -> Result<usize, KernelError> {
        self.send(ctx, buf, count, flags).await
    }

    async fn shutdown(&self, how: ShutdownHow) -> Result<(), KernelError> {
        match how {
            ShutdownHow::Read => {
                *self.rd_shutdown.lock_save_irq() = true;
            }
            ShutdownHow::Write => {
                *self.wr_shutdown.lock_save_irq() = true;
                with_net_core(|core| {
                    core.sockets
                        .get_mut::<smol_tcp::Socket>(self.handle)
                        .close();
                })?;
            }
            ShutdownHow::ReadWrite => {
                *self.rd_shutdown.lock_save_irq() = true;
                *self.wr_shutdown.lock_save_irq() = true;
                with_net_core(|core| {
                    core.sockets
                        .get_mut::<smol_tcp::Socket>(self.handle)
                        .close();
                })?;
            }
        }

        process_packets();
        Ok(())
    }

    async fn release_socket(&mut self, _ctx: &FileCtx) -> Result<(), KernelError> {
        while let Some(sock) = self.backlogs.lock_save_irq().pop() {
            sock.destroy_handle();
        }
        self.destroy_handle();
        Ok(())
    }

    fn as_file(self: Box<Self>) -> Box<dyn FileOps> {
        self
    }
}
