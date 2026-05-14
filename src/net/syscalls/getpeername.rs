use crate::memory::uaccess::{copy_from_user, copy_to_user, copy_to_user_slice};
use crate::net::SocketLen;
use crate::process::fd_table::Fd;
use crate::sched::syscall_ctx::ProcessCtx;
use libkernel::error::KernelError;
use libkernel::memory::address::{TUA, UA};

pub async fn sys_getpeername(
    ctx: &ProcessCtx,
    fd: Fd,
    addr: UA,
    addrlen: TUA<SocketLen>,
) -> libkernel::error::Result<usize> {
    if addr.is_null() || addrlen.is_null() {
        return Err(KernelError::InvalidValue);
    }

    let file = ctx
        .shared()
        .fd_table
        .lock_save_irq()
        .get(fd)
        .ok_or(KernelError::BadFd)?;

    let (ops, _ctx) = &mut *file.lock().await;
    let socket = ops.as_socket().ok_or(KernelError::NotASocket)?;
    let socket_addr = socket.getpeername().await?;

    let addrlen_val = copy_from_user(addrlen).await?;
    let bytes = socket_addr.to_bytes();
    let to_copy = bytes.len().min(addrlen_val);
    copy_to_user_slice(&bytes[..to_copy], addr).await?;
    copy_to_user(addrlen, bytes.len()).await?;
    Ok(0)
}
