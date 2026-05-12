use super::pidfs::pidfs;
use crate::clock::realtime::date;
use crate::fs::VFS;
use crate::fs::fops::FileOps;
use crate::fs::open_file::OpenFile;
use crate::process::thread_group::pid::PidT;
use crate::process::{Tid, find_task_by_tid};
use crate::sched::syscall_ctx::ProcessCtx;
use alloc::boxed::Box;
use alloc::sync::Arc;
use async_trait::async_trait;
use bitflags::bitflags;
use libkernel::error::{KernelError, Result};
use libkernel::fs::{OpenFlags, pathbuf::PathBuf};
use libkernel::memory::address::UA;
use libkernel::proc::ids::{Gid, Uid};

bitflags! {
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
    pub struct PidfdFlags: u32 {
        const PIDFD_NONBLOCK = OpenFlags::O_NONBLOCK.bits();
        const PIDFD_THREAD = OpenFlags::O_EXCL.bits();
    }
}

pub struct PidFile {
    pid: Tid,
    _flags: PidfdFlags,
}

impl PidFile {
    pub fn new(pid: Tid, flags: PidfdFlags) -> Self {
        Self { pid, _flags: flags }
    }

    pub fn new_open_file(pid: Tid, flags: PidfdFlags, uid: Uid, gid: Gid) -> Arc<OpenFile> {
        let file = PidFile::new(pid, flags);
        let mut open_file = OpenFile::new(
            Box::new(file),
            OpenFlags::O_RDWR | OpenFlags::from_bits_retain(flags.bits()),
        );
        open_file.update(pidfs().inode_for_pid(pid, uid, gid, date()), PathBuf::new());
        Arc::new(open_file)
    }

    pub fn pid(&self) -> Tid {
        self.pid
    }
}

#[async_trait]
impl FileOps for PidFile {
    async fn readat(&mut self, _buf: UA, _count: usize, _offset: u64) -> Result<usize> {
        Err(KernelError::InvalidValue)
    }

    async fn writeat(&mut self, _buf: UA, _count: usize, _offset: u64) -> Result<usize> {
        Err(KernelError::InvalidValue)
    }

    fn as_pidfd(&mut self) -> Option<&mut PidFile> {
        Some(self)
    }
}

pub async fn sys_pidfd_open(ctx: &ProcessCtx, pid: PidT, flags: u32) -> Result<usize> {
    let pid = Tid::from_pid_t(pid);
    let flags = PidfdFlags::from_bits(flags).ok_or(KernelError::InvalidValue)?;
    let task = find_task_by_tid(pid).ok_or(KernelError::NoProcess)?;
    // Ensure the pid is a thread group leader
    if !flags.contains(PidfdFlags::PIDFD_THREAD) && (Tid::from_tgid(task.process.tgid) != pid) {
        return Err(KernelError::NoProcess);
    }

    VFS.register_internal_filesystem(pidfs());

    let creds = ctx.shared().creds.lock_save_irq().clone();
    let file = PidFile::new_open_file(pid, flags, creds.uid(), creds.gid());

    let fd = ctx.task().fd_table.lock_save_irq().insert(file)?;

    Ok(fd.as_raw() as _)
}
