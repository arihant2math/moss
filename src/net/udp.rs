use crate::fs::fops::FileOps;
use crate::fs::open_file::FileCtx;
use crate::memory::uaccess::{copy_from_user_slice, copy_to_user_slice};
use crate::net::sockopt::{
    SocketMeta, SocketOptionState, SocketRuntimeInfo, get_sockopt, set_sockopt,
};
use crate::net::sops::{RecvFlags, SendFlags, SocketOps};
use crate::net::{
    AF_INET, IPPROTO_UDP, SOCK_DGRAM, ShutdownHow, SockAddr, allocate_ephemeral_port,
    infer_local_ip_for_peer, poll_network, process_packets, wait_for_network_condition,
    wait_for_network_progress, with_net_core,
};
use crate::sync::SpinLock;
use alloc::boxed::Box;
use alloc::vec;
use async_trait::async_trait;
use core::{future::Future, pin::Pin};
use libkernel::error::KernelError;
use libkernel::memory::address::UA;
use smoltcp::iface::SocketHandle;
use smoltcp::socket::udp::{self as smol_udp, PacketBuffer, PacketMetadata};
use smoltcp::wire::{IpEndpoint, IpListenEndpoint};

const UDP_PACKET_SLOTS: usize = 8;
const UDP_PAYLOAD_CAPACITY: usize = 8192;

pub struct UdpSocket {
    handle: SocketHandle,
    opts: SpinLock<SocketOptionState>,
    local_endpoint: SpinLock<Option<IpListenEndpoint>>,
    connected_peer: SpinLock<Option<IpEndpoint>>,
    rd_shutdown: SpinLock<bool>,
    wr_shutdown: SpinLock<bool>,
}

impl UdpSocket {
    pub fn new() -> Result<Self, KernelError> {
        let rx_buffer = PacketBuffer::new(
            vec![PacketMetadata::EMPTY; UDP_PACKET_SLOTS],
            vec![0; UDP_PAYLOAD_CAPACITY],
        );
        let tx_buffer = PacketBuffer::new(
            vec![PacketMetadata::EMPTY; UDP_PACKET_SLOTS],
            vec![0; UDP_PAYLOAD_CAPACITY],
        );
        let inner = smol_udp::Socket::new(rx_buffer, tx_buffer);
        let handle = with_net_core(|core| core.sockets.add(inner))?;
        Ok(Self {
            handle,
            opts: SpinLock::new(SocketOptionState::new()),
            local_endpoint: SpinLock::new(None),
            connected_peer: SpinLock::new(None),
            rd_shutdown: SpinLock::new(false),
            wr_shutdown: SpinLock::new(false),
        })
    }

    fn socket_meta(&self) -> SocketMeta {
        SocketMeta {
            domain: AF_INET,
            type_: SOCK_DGRAM,
            protocol: IPPROTO_UDP,
        }
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

    fn ensure_bound(&self) -> Result<(), KernelError> {
        let endpoint = self.normalized_local_endpoint();
        with_net_core(|core| {
            let socket = core.sockets.get_mut::<smol_udp::Socket>(self.handle);
            if socket.is_open() {
                return Ok(());
            }
            socket.bind(endpoint).map_err(|_| KernelError::InvalidValue)
        })?
    }

    fn connected_peer(&self) -> Option<IpEndpoint> {
        *self.connected_peer.lock_save_irq()
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
impl SocketOps for UdpSocket {
    async fn bind(&self, addr: SockAddr) -> Result<(), KernelError> {
        let mut endpoint: IpListenEndpoint = addr.try_into()?;
        if endpoint.port == 0 {
            endpoint.port = allocate_ephemeral_port();
        }
        *self.local_endpoint.lock_save_irq() = Some(endpoint);
        with_net_core(|core| {
            core.sockets
                .get_mut::<smol_udp::Socket>(self.handle)
                .bind(endpoint)
                .map_err(|_| KernelError::InvalidValue)
        })??;
        Ok(())
    }

    async fn connect(&self, addr: SockAddr) -> Result<(), KernelError> {
        self.ensure_bound()?;
        *self.connected_peer.lock_save_irq() = Some(addr.try_into()?);
        Ok(())
    }

    async fn recv(
        &mut self,
        ctx: &mut FileCtx,
        buf: UA,
        count: usize,
        flags: RecvFlags,
    ) -> Result<(usize, Option<SockAddr>), KernelError> {
        self.recvfrom(ctx, buf, count, flags, None).await
    }

    async fn recvfrom(
        &mut self,
        _ctx: &mut FileCtx,
        buf: UA,
        count: usize,
        flags: RecvFlags,
        _addr: Option<SockAddr>,
    ) -> Result<(usize, Option<SockAddr>), KernelError> {
        if count == 0 || *self.rd_shutdown.lock_save_irq() {
            return Ok((0, None));
        }

        let nonblock = flags.contains(RecvFlags::MSG_DONTWAIT);
        let connected_peer = self.connected_peer();

        let (data, peer) = self
            .wait_until(nonblock, || {
                with_net_core(|core| {
                    let socket = core.sockets.get_mut::<smol_udp::Socket>(self.handle);
                    if !socket.can_recv() {
                        return Ok(None);
                    }

                    let (packet, meta) = socket.recv().map_err(|_| KernelError::TryAgain)?;
                    if let Some(connected_peer) = connected_peer
                        && connected_peer != meta.endpoint
                    {
                        return Ok(None);
                    }

                    let n = packet.len().min(count);
                    let data = packet[..n].to_vec();
                    Ok(Some((data, SockAddr::from(meta.endpoint))))
                })?
            })
            .await?;

        copy_to_user_slice(&data, buf).await?;
        Ok((data.len(), Some(peer)))
    }

    async fn send(
        &mut self,
        ctx: &mut FileCtx,
        buf: UA,
        count: usize,
        flags: SendFlags,
    ) -> Result<usize, KernelError> {
        let peer = self.connected_peer().ok_or(KernelError::InvalidValue)?;
        self.sendto(ctx, buf, count, flags, SockAddr::from(peer))
            .await
    }

    async fn sendto(
        &mut self,
        _ctx: &mut FileCtx,
        buf: UA,
        count: usize,
        flags: SendFlags,
        addr: SockAddr,
    ) -> Result<usize, KernelError> {
        if count == 0 {
            return Ok(0);
        }
        if *self.wr_shutdown.lock_save_irq() {
            return Err(KernelError::BrokenPipe);
        }

        self.ensure_bound()?;
        let peer: IpEndpoint = addr.try_into()?;
        let local_address = {
            let endpoint = *self.local_endpoint.lock_save_irq();
            infer_local_ip_for_peer(endpoint.and_then(|endpoint| endpoint.addr), peer)
        };
        let metadata = smol_udp::UdpMetadata {
            endpoint: peer,
            local_address,
            meta: smoltcp::phy::PacketMeta::default(),
        };
        let mut data = vec![0u8; count];
        copy_from_user_slice(buf, &mut data).await?;
        let nonblock = flags.contains(SendFlags::MSG_DONT_WAIT);

        self.wait_until(nonblock, || {
            with_net_core(|core| {
                let socket = core.sockets.get_mut::<smol_udp::Socket>(self.handle);
                if !socket.can_send() {
                    return Ok(None);
                }
                socket
                    .send_slice(&data, metadata)
                    .map_err(|err| match err {
                        smol_udp::SendError::BufferFull => KernelError::TryAgain,
                        smol_udp::SendError::Unaddressable => KernelError::InvalidValue,
                    })?;
                Ok(Some(()))
            })?
        })
        .await?;

        process_packets();
        Ok(count)
    }

    async fn shutdown(&self, how: ShutdownHow) -> Result<(), KernelError> {
        match how {
            ShutdownHow::Read => *self.rd_shutdown.lock_save_irq() = true,
            ShutdownHow::Write => *self.wr_shutdown.lock_save_irq() = true,
            ShutdownHow::ReadWrite => {
                *self.rd_shutdown.lock_save_irq() = true;
                *self.wr_shutdown.lock_save_irq() = true;
            }
        }
        Ok(())
    }

    async fn setsockopt(
        &self,
        level: i32,
        optname: i32,
        optval: UA,
        optlen: crate::net::SocketLen,
    ) -> Result<(), KernelError> {
        set_sockopt(
            &self.opts,
            self.socket_meta(),
            level,
            optname,
            optval,
            optlen,
        )
        .await
    }

    async fn getsockopt(
        &self,
        level: i32,
        optname: i32,
        optval: UA,
        optlen: libkernel::memory::address::TUA<crate::net::SocketLen>,
    ) -> Result<(), KernelError> {
        get_sockopt(
            &self.opts,
            self.socket_meta(),
            SocketRuntimeInfo {
                accept_conn: false,
                error: 0,
            },
            level,
            optname,
            optval,
            optlen,
        )
        .await
    }

    async fn release_socket(&mut self, _ctx: &FileCtx) -> Result<(), KernelError> {
        let _ = with_net_core(|core| {
            {
                let socket = core.sockets.get_mut::<smol_udp::Socket>(self.handle);
                socket.close();
            }
            let _ = core.sockets.remove(self.handle);
        });
        Ok(())
    }

    fn poll_read_ready(&self) -> Pin<Box<dyn Future<Output = Result<(), KernelError>> + Send>> {
        let handle = self.handle;
        wait_for_network_condition(move || {
            with_net_core(|core| core.sockets.get::<smol_udp::Socket>(handle).can_recv())
        })
    }

    fn poll_write_ready(&self) -> Pin<Box<dyn Future<Output = Result<(), KernelError>> + Send>> {
        let handle = self.handle;
        wait_for_network_condition(move || {
            with_net_core(|core| core.sockets.get::<smol_udp::Socket>(handle).can_send())
        })
    }

    fn as_file(self: Box<Self>) -> Box<dyn FileOps> {
        self
    }
}
