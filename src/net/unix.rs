use crate::fs::open_file::FileCtx;
use crate::kernel::kpipe::KPipe;
use crate::memory::uaccess::{copy_from_user_slice, copy_to_user_slice};
use crate::net::sockopt::{
    SocketMeta, SocketOptionState, SocketRuntimeInfo, get_sockopt, set_sockopt,
};
use crate::net::sops::{RecvFlags, SendFlags};
use crate::net::{
    AF_UNIX, SOCK_DGRAM, SOCK_SEQPACKET, SOCK_STREAM, SockAddr, SockAddrUn, SocketOps,
};
use crate::sync::{OnceLock, SpinLock};
use alloc::boxed::Box;
use alloc::collections::{BTreeMap, VecDeque};
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use async_trait::async_trait;
use core::{
    future::{Future, poll_fn},
    pin::Pin,
    task::{Poll, Waker},
};
use libkernel::error::{FsError, KernelError, Result};
use libkernel::memory::address::UA;

struct Message {
    sender: SockAddrUn,
    data: Vec<u8>,
}

struct DatagramInbox {
    queue: VecDeque<Message>,
    waiters: Vec<Waker>,
}

#[derive(Clone)]
enum Inbox {
    Pipe(Arc<KPipe>),
    Datagram(Arc<SpinLock<DatagramInbox>>),
}

impl Inbox {
    fn new(socket_type: SocketType) -> Self {
        match socket_type {
            SocketType::Stream | SocketType::SeqPacket => {
                Inbox::Pipe(Arc::new(KPipe::new().expect("KPipe creation failed")))
            }
            SocketType::Datagram => Inbox::Datagram(Arc::new(SpinLock::new(DatagramInbox {
                queue: VecDeque::new(),
                waiters: Vec::new(),
            }))),
        }
    }

    async fn send(&self, origin: SockAddrUn, buf: UA, count: usize) -> Result<usize> {
        match self {
            Inbox::Pipe(pipe) => pipe.copy_from_user(buf, count).await,
            Inbox::Datagram(queue) => {
                let mut data = vec![0u8; count];
                copy_from_user_slice(buf, &mut data).await?;
                let msg = Message {
                    sender: origin,
                    data,
                };
                let waiters = {
                    let mut inbox = queue.lock_save_irq();
                    inbox.queue.push_back(msg);
                    core::mem::take(&mut inbox.waiters)
                };
                for waiter in waiters {
                    waiter.wake();
                }
                Ok(count)
            }
        }
    }

    async fn recv(&self, buf: UA, count: usize) -> Result<(usize, Option<SockAddrUn>)> {
        match self {
            Inbox::Pipe(pipe) => Ok((pipe.copy_to_user(buf, count).await?, None)),
            Inbox::Datagram(queue) => {
                let msg = queue.lock_save_irq().queue.pop_front();
                if let Some(msg) = msg {
                    let n = msg.data.len().min(count);
                    copy_to_user_slice(&msg.data[..n], buf).await?;
                    Ok((n, Some(msg.sender)))
                } else {
                    Ok((0, None))
                }
            }
        }
    }

    async fn send_buf(&self, origin: SockAddrUn, buf: &[u8]) -> Result<usize> {
        match self {
            Inbox::Pipe(pipe) => Ok(pipe.push_slice(buf).await),
            Inbox::Datagram(queue) => {
                let msg = Message {
                    sender: origin,
                    data: buf.to_vec(),
                };
                let waiters = {
                    let mut inbox = queue.lock_save_irq();
                    inbox.queue.push_back(msg);
                    core::mem::take(&mut inbox.waiters)
                };
                for waiter in waiters {
                    waiter.wake();
                }
                Ok(buf.len())
            }
        }
    }

    async fn recv_buf(&self, buf: &mut [u8]) -> Result<(usize, Option<SockAddrUn>)> {
        match self {
            Inbox::Pipe(pipe) => Ok((pipe.pop_slice(buf).await, None)),
            Inbox::Datagram(queue) => {
                let msg = queue.lock_save_irq().queue.pop_front();
                if let Some(msg) = msg {
                    let n = msg.data.len().min(buf.len());
                    buf[..n].copy_from_slice(&msg.data[..n]);
                    Ok((n, Some(msg.sender)))
                } else {
                    Ok((0, None))
                }
            }
        }
    }

    fn poll_read_ready(&self) -> Pin<Box<dyn Future<Output = Result<()>> + Send>> {
        match self.clone() {
            Inbox::Pipe(pipe) => Box::pin(async move {
                pipe.read_ready().await;
                Ok(())
            }),
            Inbox::Datagram(queue) => Box::pin(async move {
                poll_fn(move |cx| {
                    let mut inbox = queue.lock_save_irq();
                    if !inbox.queue.is_empty() {
                        Poll::Ready(Ok(()))
                    } else {
                        inbox.waiters.push(cx.waker().clone());
                        Poll::Pending
                    }
                })
                .await
            }),
        }
    }

    fn poll_write_ready(&self) -> Pin<Box<dyn Future<Output = Result<()>> + Send>> {
        match self.clone() {
            Inbox::Pipe(pipe) => Box::pin(async move {
                pipe.write_ready().await;
                Ok(())
            }),
            Inbox::Datagram(_) => Box::pin(async { Ok(()) }),
        }
    }
}

/// Registry mapping Unix socket path bytes to endpoint inbox and listening state
struct Endpoint {
    inbox: Inbox,
    listening: bool,
    backlog_max: usize,
    pending: Vec<UnixSocket>,
    /// Wakers for tasks waiting in accept
    waiters: Vec<Waker>,
}

/// Registry mapping Unix socket path bytes to endpoint inbox
static UNIX_ENDPOINTS: OnceLock<SpinLock<BTreeMap<Vec<u8>, Endpoint>>> = OnceLock::new();

fn endpoints() -> &'static SpinLock<BTreeMap<Vec<u8>, Endpoint>> {
    UNIX_ENDPOINTS.get_or_init(|| SpinLock::new(BTreeMap::new()))
}

#[derive(Copy, Clone)]
enum SocketType {
    Stream,
    Datagram,
    SeqPacket,
}

pub struct UnixSocket {
    socket_type: SocketType,
    opts: SpinLock<SocketOptionState>,
    /// Recv inbox
    inbox: Inbox,
    /// The peer endpoint's inbox
    peer_inbox: SpinLock<Option<Inbox>>,
    local_addr: SpinLock<Option<SockAddrUn>>,
    peer_addr: SpinLock<Option<SockAddrUn>>,
    connected: SpinLock<bool>,
    listening: SpinLock<bool>,
    backlog: SpinLock<usize>,
    // Shutdown state
    rd_shutdown: SpinLock<bool>,
    wr_shutdown: SpinLock<bool>,
}

impl UnixSocket {
    fn new(socket_type: SocketType) -> Self {
        UnixSocket {
            socket_type,
            opts: SpinLock::new(SocketOptionState::new()),
            inbox: Inbox::new(socket_type),
            peer_inbox: SpinLock::new(None),
            local_addr: SpinLock::new(None),
            peer_addr: SpinLock::new(None),
            connected: SpinLock::new(false),
            listening: SpinLock::new(false),
            backlog: SpinLock::new(0),
            rd_shutdown: SpinLock::new(false),
            wr_shutdown: SpinLock::new(false),
        }
    }

    fn socket_meta(&self) -> SocketMeta {
        SocketMeta {
            domain: AF_UNIX,
            type_: match self.socket_type {
                SocketType::Stream => SOCK_STREAM,
                SocketType::Datagram => SOCK_DGRAM,
                SocketType::SeqPacket => SOCK_SEQPACKET,
            },
            protocol: 0,
        }
    }

    pub fn new_stream() -> Self {
        Self::new(SocketType::Stream)
    }
    pub fn new_datagram() -> Self {
        Self::new(SocketType::Datagram)
    }
    pub fn new_seqpacket() -> Self {
        Self::new(SocketType::SeqPacket)
    }

    fn unnamed_addr() -> SockAddrUn {
        SockAddrUn {
            family: AF_UNIX as u16,
            path: [0; 108],
        }
    }

    fn path_bytes(saun: &crate::net::SockAddrUn) -> Option<Vec<u8>> {
        // Unix path is a sun_path-like fixed-size buffer which may be null-terminated
        let mut end = saun.path.len();
        for (i, b) in saun.path.iter().enumerate() {
            if *b == 0 {
                end = i;
                break;
            }
        }
        if end == 0 {
            None
        } else {
            Some(saun.path[..end].to_vec())
        }
    }
}

#[async_trait]
impl SocketOps for UnixSocket {
    async fn bind(&self, addr: SockAddr) -> Result<()> {
        match addr {
            SockAddr::Un(saun) => {
                let Some(path) = UnixSocket::path_bytes(&saun) else {
                    return Err(KernelError::InvalidValue);
                };
                // Register endpoint; if already exists, return error
                let mut reg = endpoints().lock_save_irq();
                if reg.contains_key(&path) {
                    return Err(KernelError::InvalidValue);
                }
                reg.insert(
                    path,
                    Endpoint {
                        inbox: self.inbox.clone(),
                        listening: false,
                        backlog_max: 4096,
                        pending: Vec::new(),
                        waiters: Vec::new(),
                    },
                );
                *self.local_addr.lock_save_irq() = Some(saun);
                Ok(())
            }
            _ => Err(KernelError::InvalidValue),
        }
    }

    async fn connect(&self, addr: SockAddr) -> Result<()> {
        match addr {
            SockAddr::Un(saun) => {
                let Some(path) = UnixSocket::path_bytes(&saun) else {
                    return Err(KernelError::InvalidValue);
                };
                let local_addr = (*self.local_addr.lock_save_irq()).unwrap_or(Self::unnamed_addr());
                let mut reg = endpoints().lock_save_irq();
                let Some(ep) = reg.get_mut(&path) else {
                    return Err(KernelError::Fs(FsError::NotFound));
                };
                if ep.listening {
                    if ep.pending.len() >= ep.backlog_max {
                        return Err(KernelError::TryAgain);
                    }
                    let server_sock = UnixSocket::new(SocketType::Stream);
                    // For accepted sockets, local address matches the listening path (Linux getsockname).
                    *server_sock.local_addr.lock_save_irq() = Some(saun);
                    *server_sock.peer_addr.lock_save_irq() = Some(local_addr);
                    *server_sock.peer_inbox.lock_save_irq() = Some(self.inbox.clone());
                    *server_sock.connected.lock_save_irq() = true;

                    // Client links to accepted socket inbox.
                    *self.peer_inbox.lock_save_irq() = Some(server_sock.inbox.clone());
                    *self.peer_addr.lock_save_irq() = Some(saun);
                    *self.connected.lock_save_irq() = true;

                    ep.pending.push(server_sock);
                    // Wake one waiter if present
                    if let Some(w) = ep.waiters.pop() {
                        w.wake();
                    }
                    Ok(())
                } else {
                    // Non-listening endpoint: treat as datagram or pre-bound stream endpoint
                    *self.peer_inbox.lock_save_irq() = Some(ep.inbox.clone());
                    *self.peer_addr.lock_save_irq() = Some(saun);
                    *self.connected.lock_save_irq() = true;
                    Ok(())
                }
            }
            _ => Err(KernelError::InvalidValue),
        }
    }

    async fn listen(&self, mut backlog: i32) -> Result<()> {
        match self.socket_type {
            SocketType::Stream | SocketType::SeqPacket => {}
            SocketType::Datagram => return Err(KernelError::NotSupported),
        }
        if backlog <= 0 {
            backlog = 4096;
        }
        let Some(saun) = &*self.local_addr.lock_save_irq() else {
            return Err(KernelError::InvalidValue);
        };
        let Some(path) = UnixSocket::path_bytes(saun) else {
            return Err(KernelError::InvalidValue);
        };
        let mut reg = endpoints().lock_save_irq();
        let Some(ep) = reg.get_mut(&path) else {
            return Err(KernelError::InvalidValue);
        };
        ep.listening = true;
        ep.backlog_max = backlog as usize;
        *self.listening.lock_save_irq() = true;
        *self.backlog.lock_save_irq() = backlog as usize;
        Ok(())
    }

    async fn accept(&self) -> Result<(Box<dyn SocketOps>, SockAddr)> {
        {
            if !*self.listening.lock_save_irq() {
                return Err(KernelError::InvalidValue);
            }
        }
        let path_vec: Vec<u8> = {
            let guard = self.local_addr.lock_save_irq();
            let Some(saun) = &*guard else {
                return Err(KernelError::InvalidValue);
            };
            let Some(pv) = UnixSocket::path_bytes(saun) else {
                return Err(KernelError::InvalidValue);
            };
            pv
        };

        let sock = poll_fn(|cx| {
            let mut reg = endpoints().lock_save_irq();
            let Some(ep) = reg.get_mut(&path_vec) else {
                return Poll::Ready(Err(KernelError::InvalidValue));
            };
            // Linux accept dequeues in FIFO order.
            if !ep.pending.is_empty() {
                let sock = ep.pending.remove(0);
                Poll::Ready(Ok(sock))
            } else {
                ep.waiters.push(cx.waker().clone());
                Poll::Pending
            }
        })
        .await?;

        let peer_addr = (*sock.peer_addr.lock_save_irq())
            .map(SockAddr::Un)
            .ok_or(KernelError::NotConnected)?;

        Ok((Box::new(sock), peer_addr))
    }

    async fn recv(
        &mut self,
        _ctx: &mut FileCtx,
        buf: UA,
        count: usize,
        _flags: RecvFlags,
    ) -> Result<(usize, Option<SockAddr>)> {
        if count == 0 {
            return Ok((0, None));
        }
        if *self.rd_shutdown.lock_save_irq() {
            return Ok((0, None));
        }
        self.inbox.recv(buf, count).await.map(|(n, peer)| {
            let peer_addr = peer.map(SockAddr::Un);
            (n, peer_addr)
        })
    }

    async fn recvfrom(
        &mut self,
        ctx: &mut FileCtx,
        buf: UA,
        count: usize,
        flags: RecvFlags,
        _addr: Option<SockAddr>,
    ) -> Result<(usize, Option<SockAddr>)> {
        let n = self.recv(ctx, buf, count, flags).await?;
        Ok(n)
    }

    async fn recvfrom_buf(
        &mut self,
        _ctx: &mut FileCtx,
        buf: &mut [u8],
        _flags: RecvFlags,
        _addr: Option<SockAddr>,
    ) -> Result<(usize, Option<SockAddr>)> {
        if buf.is_empty() {
            return Ok((0, None));
        }
        if *self.rd_shutdown.lock_save_irq() {
            return Ok((0, None));
        }
        self.inbox.recv_buf(buf).await.map(|(n, peer)| {
            let peer_addr = peer.map(SockAddr::Un);
            (n, peer_addr)
        })
    }

    async fn send(
        &mut self,
        _ctx: &mut FileCtx,
        buf: UA,
        count: usize,
        _flags: SendFlags,
    ) -> Result<usize> {
        if count == 0 {
            return Ok(0);
        }
        if *self.wr_shutdown.lock_save_irq() {
            return Err(KernelError::BrokenPipe);
        }
        match self.socket_type {
            SocketType::Stream | SocketType::SeqPacket => {
                if !*self.connected.lock_save_irq() {
                    return Err(KernelError::InvalidValue);
                }
            }
            SocketType::Datagram => {}
        }
        let Some(peer) = self.peer_inbox.lock_save_irq().clone() else {
            return Err(KernelError::InvalidValue);
        };
        let local_addr = self
            .local_addr
            .lock_save_irq()
            .unwrap_or(Self::unnamed_addr());
        peer.send(local_addr, buf, count).await
    }

    async fn sendto(
        &mut self,
        _ctx: &mut FileCtx,
        buf: UA,
        count: usize,
        _flags: SendFlags,
        addr: SockAddr,
    ) -> Result<usize> {
        let peer_inbox = match addr {
            SockAddr::Un(saun) => {
                let Some(path) = UnixSocket::path_bytes(&saun) else {
                    return Err(KernelError::InvalidValue);
                };
                let reg = endpoints().lock_save_irq();
                let Some(ep) = reg.get(&path) else {
                    return Err(KernelError::Fs(FsError::NotFound));
                };
                ep.inbox.clone()
            }
            _ => return Err(KernelError::InvalidValue),
        };
        let local_addr = self
            .local_addr
            .lock_save_irq()
            .unwrap_or(Self::unnamed_addr());
        peer_inbox.send(local_addr, buf, count).await
    }

    async fn sendto_buf(
        &mut self,
        _ctx: &mut FileCtx,
        buf: &[u8],
        _flags: SendFlags,
        addr: Option<SockAddr>,
    ) -> Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        if *self.wr_shutdown.lock_save_irq() {
            return Err(KernelError::BrokenPipe);
        }

        let local_addr = self
            .local_addr
            .lock_save_irq()
            .unwrap_or(Self::unnamed_addr());

        let peer_inbox = match addr {
            Some(SockAddr::Un(saun)) => {
                let Some(path) = UnixSocket::path_bytes(&saun) else {
                    return Err(KernelError::InvalidValue);
                };
                let reg = endpoints().lock_save_irq();
                let Some(ep) = reg.get(&path) else {
                    return Err(KernelError::Fs(FsError::NotFound));
                };
                ep.inbox.clone()
            }
            Some(_) => return Err(KernelError::InvalidValue),
            None => {
                match self.socket_type {
                    SocketType::Stream | SocketType::SeqPacket => {
                        if !*self.connected.lock_save_irq() {
                            return Err(KernelError::InvalidValue);
                        }
                    }
                    SocketType::Datagram => {}
                }
                self.peer_inbox
                    .lock_save_irq()
                    .clone()
                    .ok_or(KernelError::InvalidValue)?
            }
        };

        peer_inbox.send_buf(local_addr, buf).await
    }

    async fn getsockname(&self) -> Result<SockAddr> {
        Ok(SockAddr::Un(
            (*self.local_addr.lock_save_irq()).unwrap_or(Self::unnamed_addr()),
        ))
    }

    async fn getpeername(&self) -> Result<SockAddr> {
        (*self.peer_addr.lock_save_irq())
            .map(SockAddr::Un)
            .ok_or(KernelError::NotConnected)
    }

    async fn shutdown(&self, how: crate::net::ShutdownHow) -> Result<()> {
        match how {
            crate::net::ShutdownHow::Read => {
                *self.rd_shutdown.lock_save_irq() = true;
            }
            crate::net::ShutdownHow::Write => {
                *self.wr_shutdown.lock_save_irq() = true;
            }
            crate::net::ShutdownHow::ReadWrite => {
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
    ) -> Result<()> {
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
    ) -> Result<()> {
        let accept_conn = *self.listening.lock_save_irq();
        get_sockopt(
            &self.opts,
            self.socket_meta(),
            SocketRuntimeInfo {
                accept_conn,
                error: 0,
            },
            level,
            optname,
            optval,
            optlen,
        )
        .await
    }

    fn poll_read_ready(&self) -> Pin<Box<dyn Future<Output = Result<()>> + Send>> {
        if *self.listening.lock_save_irq() {
            let Some(saun) = *self.local_addr.lock_save_irq() else {
                return Box::pin(async { Err(KernelError::InvalidValue) });
            };
            let Some(path_vec) = UnixSocket::path_bytes(&saun) else {
                return Box::pin(async { Err(KernelError::InvalidValue) });
            };

            return Box::pin(async move {
                poll_fn(move |cx| {
                    let mut reg = endpoints().lock_save_irq();
                    let Some(ep) = reg.get_mut(&path_vec) else {
                        return Poll::Ready(Err(KernelError::InvalidValue));
                    };
                    if !ep.pending.is_empty() {
                        Poll::Ready(Ok(()))
                    } else {
                        ep.waiters.push(cx.waker().clone());
                        Poll::Pending
                    }
                })
                .await
            });
        }

        self.inbox.poll_read_ready()
    }

    fn poll_write_ready(&self) -> Pin<Box<dyn Future<Output = Result<()>> + Send>> {
        match self.socket_type {
            SocketType::Datagram => Box::pin(async { Ok(()) }),
            SocketType::Stream | SocketType::SeqPacket => self
                .peer_inbox
                .lock_save_irq()
                .clone()
                .map(|inbox| inbox.poll_write_ready())
                .unwrap_or_else(|| Box::pin(async { Ok(()) })),
        }
    }

    fn as_file(self: Box<Self>) -> Box<dyn crate::fs::fops::FileOps> {
        self
    }
}
