use crate::net::SocketLen;
use crate::process::fd_table::Fd;
use crate::sched::syscall_ctx::ProcessCtx;
use libkernel::error::KernelError;
use libkernel::memory::address::{TUA, UA};

pub async fn sys_getsockopt(
    ctx: &ProcessCtx,
    fd: Fd,
    level: i32,
    optname: i32,
    optval: UA,
    optlen: TUA<SocketLen>,
) -> libkernel::error::Result<usize> {
    let file = ctx
        .shared()
        .fd_table
        .lock_save_irq()
        .get(fd)
        .ok_or(KernelError::BadFd)?;

    let (ops, _ctx) = &mut *file.lock().await;
    ops.as_socket()
        .ok_or(KernelError::NotASocket)?
        .getsockopt(level, optname, optval, optlen)
        .await?;
    Ok(0)
}
