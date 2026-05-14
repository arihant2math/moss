use crate::fs::syscalls::iov::IoVec;
use crate::memory::uaccess::{
    UserCopyable, copy_from_user, copy_from_user_slice, copy_obj_array_from_user, copy_to_user,
    copy_to_user_slice,
};
use crate::net::sops::{RecvFlags, SendFlags};
use crate::net::{SockAddr, parse_sockaddr};
use crate::process::fd_table::Fd;
use crate::sched::syscall_ctx::ProcessCtx;
use alloc::vec;
use alloc::vec::Vec;
use libkernel::error::{KernelError, Result};
use libkernel::memory::address::TUA;

#[derive(Clone, Copy)]
#[repr(C)]
pub struct MsgHdr {
    pub msg_name: TUA<u8>,
    pub msg_namelen: u32,
    pub msg_iov: TUA<IoVec>,
    pub msg_iovlen: usize,
    pub msg_control: TUA<u8>,
    pub msg_controllen: usize,
    pub msg_flags: i32,
}

// SAFETY: MsgHdr is a plain user ABI structure made only of user pointers and POD fields.
unsafe impl UserCopyable for MsgHdr {}

fn send_flags_from_bits(flags: i32) -> SendFlags {
    let bits = flags as u32;
    if flags != 0 && SendFlags::from_bits(bits).is_none() {
        log::warn!("sys_sendmsg: ignoring unsupported flags: {flags}");
    }
    SendFlags::from_bits_truncate(bits)
}

fn recv_flags_from_bits(flags: i32) -> RecvFlags {
    let bits = flags as u32;
    if flags != 0 && RecvFlags::from_bits(bits).is_none() {
        log::warn!("sys_recvmsg: ignoring unsupported flags: {flags}");
    }
    RecvFlags::from_bits_truncate(bits)
}

async fn copy_iovecs(msghdr: &MsgHdr) -> Result<Vec<IoVec>> {
    if msghdr.msg_iovlen == 0 {
        return Ok(Vec::new());
    }

    copy_obj_array_from_user(msghdr.msg_iov, msghdr.msg_iovlen).await
}

fn total_iov_len(iovs: &[IoVec]) -> Result<usize> {
    let mut total = 0usize;
    for iov in iovs {
        total = total
            .checked_add(iov.iov_len)
            .ok_or(KernelError::InvalidValue)?;
    }
    Ok(total)
}

async fn flatten_iovecs(iovs: &[IoVec]) -> Result<Vec<u8>> {
    let total = total_iov_len(iovs)?;
    let mut data = vec![0u8; total];
    let mut offset = 0;

    for iov in iovs {
        if iov.iov_len == 0 {
            continue;
        }

        let end = offset + iov.iov_len;
        copy_from_user_slice(iov.iov_base, &mut data[offset..end]).await?;
        offset = end;
    }

    Ok(data)
}

async fn scatter_iovecs(iovs: &[IoVec], data: &[u8]) -> Result<()> {
    let mut offset = 0usize;

    for iov in iovs {
        if offset == data.len() {
            break;
        }
        if iov.iov_len == 0 {
            continue;
        }

        let chunk_len = (data.len() - offset).min(iov.iov_len);
        let end = offset + chunk_len;
        copy_to_user_slice(&data[offset..end], iov.iov_base).await?;
        offset = end;
    }

    Ok(())
}

async fn parse_msg_name(msghdr: &MsgHdr) -> Result<Option<SockAddr>> {
    if msghdr.msg_name.is_null() || msghdr.msg_namelen == 0 {
        return Ok(None);
    }

    parse_sockaddr(msghdr.msg_name.to_untyped(), msghdr.msg_namelen as usize)
        .await
        .map(Some)
}

pub async fn sys_sendmsg(
    ctx: &ProcessCtx,
    fd: Fd,
    msghdr: TUA<MsgHdr>,
    flags: i32,
) -> Result<usize> {
    let file = ctx
        .shared()
        .fd_table
        .lock_save_irq()
        .get(fd)
        .ok_or(KernelError::BadFd)?;
    let msghdr = copy_from_user(msghdr).await?;
    if msghdr.msg_controllen != 0 {
        log::warn!("sys_sendmsg: ancillary data is not supported yet");
        return Err(KernelError::NotSupported);
    }

    let iovs = copy_iovecs(&msghdr).await?;
    let data = flatten_iovecs(&iovs).await?;
    let addr = parse_msg_name(&msghdr).await?;
    let flags = send_flags_from_bits(flags);

    let (ops, ctx) = &mut *file.lock().await;
    let socket = ops.as_socket().ok_or(KernelError::NotASocket)?;
    socket.sendto_buf(ctx, &data, flags, addr).await
}

pub async fn sys_recvmsg(
    ctx: &ProcessCtx,
    fd: Fd,
    msghdr_ptr: TUA<MsgHdr>,
    flags: i32,
) -> Result<usize> {
    let file = ctx
        .shared()
        .fd_table
        .lock_save_irq()
        .get(fd)
        .ok_or(KernelError::BadFd)?;
    let mut msghdr = copy_from_user(msghdr_ptr).await?;
    let iovs = copy_iovecs(&msghdr).await?;
    let total_len = total_iov_len(&iovs)?;
    let mut data = vec![0u8; total_len];
    let flags = recv_flags_from_bits(flags);

    let (ops, ctx) = &mut *file.lock().await;
    let socket = ops.as_socket().ok_or(KernelError::NotASocket)?;
    let (message_len, recv_addr) = socket.recvfrom_buf(ctx, &mut data, flags, None).await?;
    scatter_iovecs(&iovs, &data[..message_len]).await?;

    if let Some(recv_addr) = recv_addr {
        if !msghdr.msg_name.is_null() {
            let bytes = recv_addr.to_bytes();
            let to_copy = bytes.len().min(msghdr.msg_namelen as usize);
            copy_to_user_slice(&bytes[..to_copy], msghdr.msg_name.to_untyped()).await?;
            msghdr.msg_namelen = bytes.len() as u32;
        } else {
            msghdr.msg_namelen = 0;
        }
    } else {
        msghdr.msg_namelen = 0;
    }

    msghdr.msg_controllen = 0;
    msghdr.msg_flags = 0;
    copy_to_user(msghdr_ptr, msghdr).await?;

    Ok(message_len)
}
