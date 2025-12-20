use alloc::boxed::Box;
use async_trait::async_trait;
use crate::net::{DatagramSocket, Socket, StreamSocket};
use crate::net::sopts::{Shutdown, SockAddr};
use crate::sync::Mutex;

pub enum SocketType {
    Datagram(Box<dyn DatagramSocket>),
    Stream(Box<dyn StreamSocket>),
}

#[async_trait]
impl Socket for SocketType {
    async fn bind(&mut self, addr: &SockAddr) -> libkernel::error::Result<()> {
        match self {
            SocketType::Datagram(sock) => sock.bind(addr).await,
            SocketType::Stream(sock) => sock.bind(addr).await
        }
    }

    async fn connect(&mut self, addr: &SockAddr) -> libkernel::error::Result<()> {
        match self {
            SocketType::Datagram(sock) => sock.connect(addr).await,
            SocketType::Stream(sock) => sock.connect(addr).await
        }
    }

    async fn local_addr(&self) -> libkernel::error::Result<SockAddr> {
        match self {
            SocketType::Datagram(sock) => sock.local_addr().await,
            SocketType::Stream(sock) => sock.local_addr().await
        }
    }

    async fn peer_addr(&self) -> libkernel::error::Result<SockAddr> {
        match self {
            SocketType::Datagram(sock) => sock.peer_addr().await,
            SocketType::Stream(sock) => sock.peer_addr().await
        }
    }

    async fn shutdown(&mut self, how: Shutdown) -> libkernel::error::Result<()> {
        match self {
            SocketType::Datagram(sock) => sock.shutdown(how).await,
            SocketType::Stream(sock) => sock.shutdown(how).await
        }
    }

    async fn close(&mut self) -> libkernel::error::Result<()> {
        match self {
            SocketType::Datagram(sock) => sock.close().await,
            SocketType::Stream(sock) => sock.close().await
        }
    }
}

pub struct OpenSocket {
    inner: Mutex<SocketType>,
}

impl OpenSocket {
    pub fn new(ops: SocketType) -> Self {
        Self {
            inner: Mutex::new(ops),
        }
    }

    pub async fn lock(&self) -> impl core::ops::DerefMut<Target = SocketType> + '_ {
        self.inner.lock().await
    }
}
