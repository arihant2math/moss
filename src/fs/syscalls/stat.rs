use super::at::stat::Stat;
use crate::memory::uaccess::copy_to_user;
use crate::{clock::realtime::date, process::fd_table::Fd, sched::syscall_ctx::ProcessCtx};
use libkernel::error::{KernelError, Result};
use libkernel::{
    fs::{FileType, InodeId, attr::{FileAttr, FilePermissions}},
    memory::PAGE_SIZE,
    memory::address::TUA,
};

pub async fn sys_fstat(ctx: &ProcessCtx, fd: Fd, statbuf: TUA<Stat>) -> Result<usize> {
    let fd = ctx
        .shared()
        .fd_table
        .lock_save_irq()
        .get(fd)
        .ok_or(KernelError::BadFd)?;

    let attr = if let Some(inode) = fd.inode() {
        inode.getattr().await?
    } else {
        let creds = ctx.shared().creds.lock_save_irq().clone();
        let now = date();
        FileAttr {
            id: InodeId::dummy(),
            size: 0,
            block_size: PAGE_SIZE as _,
            blocks: 0,
            atime: now,
            btime: now,
            mtime: now,
            ctime: now,
            file_type: FileType::File,
            permissions: FilePermissions::from_bits_retain(0o600),
            nlinks: 1,
            uid: creds.uid(),
            gid: creds.gid(),
        }
    };

    copy_to_user(statbuf, attr.into()).await?;

    Ok(0)
}
