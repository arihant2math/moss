use alloc::boxed::Box;
use async_trait::async_trait;
use smoltcp::socket::tcp;
use crate::net::{Shutdown, Socket};
use crate::net::sopts::SockAddr;

#[async_trait]
impl Socket for tcp::Socket<'_> {
    async fn bind(&mut self, addr: &SockAddr) -> libkernel::error::Result<()> {
        todo!();
    }

    async fn connect(&mut self, addr: &SockAddr) -> libkernel::error::Result<()> {
        todo!()
    }

    async fn local_addr(&self) -> libkernel::error::Result<SockAddr> {
        todo!()
    }

    async fn peer_addr(&self) -> libkernel::error::Result<SockAddr> {
        todo!()
    }

    async fn shutdown(&mut self, how: Shutdown) -> libkernel::error::Result<()> {
        self.abort();
        Ok(())
    }

    async fn close(&mut self) -> libkernel::error::Result<()> {
        self.close();
        Ok(())
    }
}