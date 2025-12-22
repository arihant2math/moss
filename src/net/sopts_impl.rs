use alloc::boxed::Box;
use async_trait::async_trait;
use core::net::{Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV4, SocketAddrV6};

use libkernel::error::KernelError;
use smoltcp::socket::tcp;
use smoltcp::wire::{IpAddress, IpEndpoint, IpListenEndpoint};

use crate::net::sopts::{SockAddr, StreamSocket};
use crate::net::{Shutdown, Socket};

/// Thin wrapper that exposes `smoltcp::socket::tcp::Socket` through the high-level
/// `Socket`/`StreamSocket` traits expected by the kernel.
///
/// The primary goal is to make the socket usable from the generic `sys_socket`,
/// `sys_bind`, `sys_listen`, … syscalls while still delegating all protocol work
/// to smoltcp.  Functionality that is not yet required (e.g. `accept`,
/// fully-featured `connect`) is stubbed with `KernelError::NotSupported`.
pub struct TcpSocket<'a> {
    inner: tcp::Socket<'a>,
}

impl<'a> TcpSocket<'a> {
    pub fn new(inner: tcp::Socket<'a>) -> Self {
        Self { inner }
    }

    /// Helper: convert smoltcp `IpEndpoint` → `core::net::SocketAddr`.
    fn ip_endpoint_to_std(ep: IpEndpoint) -> SocketAddr {
        match ep.addr {
            IpAddress::Ipv4(v4) => SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::from(v4), ep.port)),
            IpAddress::Ipv6(v6) => {
                SocketAddr::V6(SocketAddrV6::new(Ipv6Addr::from(v6), ep.port, 0, 0))
            }
        }
    }

    /// Helper: convert `core::net::SocketAddr` → smoltcp `ListenEndpoint`.
    fn std_to_listen_endpoint(sa: SocketAddr) -> IpListenEndpoint {
        match sa {
            SocketAddr::V4(v4) => IpListenEndpoint {
                addr: Some(IpAddress::Ipv4(*v4.ip())),
                port: v4.port(),
            },
            SocketAddr::V6(v6) => IpListenEndpoint {
                addr: Some(IpAddress::Ipv6(*v6.ip())),
                port: v6.port(),
            },
        }
    }
}

#[async_trait]
impl<'a> Socket for TcpSocket<'a> {
    async fn bind(&mut self, addr: &SockAddr) -> libkernel::error::Result<()> {
        let SockAddr::Inet(sa) = addr else {
            return Err(KernelError::NotSupported);
        };

        let ep = Self::std_to_listen_endpoint(*sa);
        if ep.port == 0 {
            return Err(KernelError::InvalidValue);
        }

        self.inner.listen(ep).map_err(|_| KernelError::NotSupported)
    }

    async fn connect(&mut self, _addr: &SockAddr) -> libkernel::error::Result<()> {
        // smoltcp `connect` needs an `InterfaceContext`.  Until the kernel grows a
        // proper network interface abstraction we leave this unimplemented.
        Err(KernelError::NotSupported)
    }

    async fn local_addr(&self) -> libkernel::error::Result<SockAddr> {
        let Some(ep) = self.inner.local_endpoint() else {
            return Err(KernelError::InvalidValue);
        };
        Ok(SockAddr::Inet(Self::ip_endpoint_to_std(ep)))
    }

    async fn peer_addr(&self) -> libkernel::error::Result<SockAddr> {
        let Some(ep) = self.inner.remote_endpoint() else {
            return Err(KernelError::InvalidValue);
        };
        Ok(SockAddr::Inet(Self::ip_endpoint_to_std(ep)))
    }

    async fn shutdown(&mut self, _how: Shutdown) -> libkernel::error::Result<()> {
        self.inner.abort();
        Ok(())
    }

    async fn close(&mut self) -> libkernel::error::Result<()> {
        self.inner.close();
        Ok(())
    }
}

#[async_trait]
impl<'a> StreamSocket for TcpSocket<'a> {
    async fn listen(&mut self, _backlog: u32) -> libkernel::error::Result<()> {
        // `bind` already transitioned the socket into LISTEN; nothing extra to do.
        Ok(())
    }

    async fn accept(&mut self) -> libkernel::error::Result<(Box<dyn StreamSocket>, SockAddr)> {
        // Proper accept requires a passive listening socket spawning a new active
        // socket.  Not wired up yet.
        Err(KernelError::NotSupported)
    }

    async fn send(&mut self, buf: &[u8]) -> libkernel::error::Result<usize> {
        self.inner
            .send_slice(buf)
            .map_err(|_| KernelError::NotSupported)
    }

    async fn recv(&mut self, buf: &mut [u8]) -> libkernel::error::Result<usize> {
        self.inner
            .recv_slice(buf)
            .map_err(|_| KernelError::NotSupported)
    }
}
