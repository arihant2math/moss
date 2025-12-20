mod open_socket;
mod sopts;
mod sopts_impl;

use libkernel::memory::address::TUA;
pub use open_socket::OpenSocket;
pub use sopts::{Socket, StreamSocket, DatagramSocket, Shutdown};
use crate::sched::current_task;

pub struct SocketAddr {
    sa_family: u32,
    sa_data: [char; 14]
}

pub async fn sys_socket(family: i32, type_: i32, protocol: i32) -> i32 {
    
    // current_task().fd_table.lock_save_irq().insert(socket)?;
}

pub async fn sys_bind(fd: i32, socket_addr: TUA<SocketAddr>, addrlen: i32) -> i32 {
}

pub async fn sys_listen(fd: i32, backlog: i32) -> i32 {
}
