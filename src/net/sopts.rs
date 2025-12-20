// src/net/sopts.rs

use alloc::boxed::Box;
use alloc::vec::Vec;

pub enum Shutdown {
    Read,
    Write,
    Both,
}

pub enum SockAddr {
    Inet(core::net::SocketAddr),
    Unix(Vec<u8>), // or a dedicated UnixAddr
}

pub struct RecvMeta {
    pub addr: SockAddr,
    pub truncated: bool, // MSG_TRUNC-like
}

pub trait SockOpt {
    type Input;
    type Output;
    fn name(&self) -> (i32, i32); // (level, optname) for BSD-like mapping
}

// Base socket operations (common to all types)
#[async_trait::async_trait]
pub trait Socket: Send + Sync {
    async fn bind(&mut self, addr: &SockAddr) -> libkernel::error::Result<()>;
    async fn connect(&mut self, addr: &SockAddr) -> libkernel::error::Result<()>;

    async fn local_addr(&self) -> libkernel::error::Result<SockAddr>;
    async fn peer_addr(&self) -> libkernel::error::Result<SockAddr>;

    // async fn setsockopt<T: SockOpt>(&mut self, opt: T, val: T::Input) -> libkernel::error::Result<()>;
    // async fn getsockopt<T: SockOpt>(&self, opt: T) -> libkernel::error::Result<T::Output>;

    async fn shutdown(&mut self, how: Shutdown) -> libkernel::error::Result<()>;
    async fn close(&mut self) -> libkernel::error::Result<()>;
}

// Stream (SOCK_STREAM) operations
#[async_trait::async_trait]
pub trait StreamSocket: Socket {
    async fn listen(&mut self, backlog: u32) -> libkernel::error::Result<()>;
    async fn accept(&mut self) -> libkernel::error::Result<(Box<dyn StreamSocket>, SockAddr)>;

    async fn send(&mut self, buf: &[u8]) -> libkernel::error::Result<usize>;
    async fn recv(&mut self, buf: &mut [u8]) -> libkernel::error::Result<usize>;
}

// Datagram (SOCK_DGRAM) operations
#[async_trait::async_trait]
pub trait DatagramSocket: Socket {
    async fn sendto(&mut self, buf: &[u8], addr: &SockAddr) -> libkernel::error::Result<usize>;
    async fn recvfrom(&mut self, buf: &mut [u8]) -> libkernel::error::Result<RecvMeta>;
}
