mod open_socket;
mod sopts;
mod sopts_impl;

use alloc::boxed::Box;
use crate::sched::current_task;
use core::cmp::PartialEq;
use libkernel::error::KernelError;
use libkernel::memory::address::TUA;
pub use open_socket::OpenSocket;
pub use sopts::{DatagramSocket, Shutdown, Socket, StreamSocket};

pub struct SocketAddr {
    sa_family: u32,
    sa_data: [char; 14],
}

const AF_INET: i32 = 2;
const AF_INET6: i32 = 10;

#[repr(u8)]
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum SocketType {
    Dgram = 1,
    Stream = 2,
    Raw = 3,
    Rdm = 4,
    SeqPacket = 5,
    Dccp = 6,
    Packet = 10,
}

impl TryFrom<i32> for SocketType {
    type Error = ();

    fn try_from(value: i32) -> Result<Self, Self::Error> {
        match value {
            1 => Ok(SocketType::Dgram),
            2 => Ok(SocketType::Stream),
            3 => Ok(SocketType::Raw),
            4 => Ok(SocketType::Rdm),
            5 => Ok(SocketType::SeqPacket),
            6 => Ok(SocketType::Dccp),
            10 => Ok(SocketType::Packet),
            _ => Err(()),
        }
    }
}

pub async fn sys_socket(family: i32, type_: i32, protocol: i32) -> libkernel::error::Result<i32> {
    let socket_type = match SocketType::try_from(type_) {
        Ok(t) => t,
        Err(_) => return Err(KernelError::InvalidValue),
    };
    if family != AF_INET && family != AF_INET6 {
        return Err(KernelError::InvalidValue);
    }
    if socket_type != SocketType::Stream {
        return Err(KernelError::InvalidValue);
    }

    use crate::net::sopts_impl::TcpSocket;
    use alloc::{sync::Arc, vec::Vec};
    use smoltcp::socket::tcp;

    // 4 KiB RX/TX buffers
    // TODO: Expandable buffers?
    let rx_buf = tcp::SocketBuffer::new(Vec::with_capacity(4096));
    let tx_buf = tcp::SocketBuffer::new(Vec::with_capacity(4096));
    let smol = tcp::Socket::new(rx_buf, tx_buf);
    let tcp_socket = TcpSocket::new(smol);

    // Wrap the socket for dynamic dispatch and place it in an `OpenSocket`.
    let open_socket = Arc::new(OpenSocket::new(open_socket::SocketType::Stream(Box::new(
        tcp_socket,
    ))));

    // Insert the socket into the current task’s FD table.
    let fd = current_task()
        .fd_table
        .lock_save_irq()
        .insert(open_socket)?;

    Ok(fd.as_raw())
}

pub async fn sys_bind(_fd: i32, _socket_addr: TUA<SocketAddr>, _addrlen: i32) -> libkernel::error::Result<i32> {
    // TODO: Implement address translation & smoltcp binding.
    // Until networking is fully wired up, signal “not supported”.
    Err(libkernel::error::KernelError::NotSupported)
}

pub async fn sys_listen(_fd: i32, _backlog: i32) -> libkernel::error::Result<i32> {
    // TODO: Implement listen handling once accept is available.
    Err(libkernel::error::KernelError::NotSupported)
}
