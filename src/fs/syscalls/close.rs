use crate::net::{Shutdown, Socket};
use crate::process::fd_table::FileDescriptorEntryInner;
use crate::{process::fd_table::Fd, sched::current_task};
use alloc::sync::Arc;
use libkernel::error::{KernelError, Result};

pub async fn sys_close(fd: Fd) -> Result<usize> {
    let file = current_task()
        .fd_table
        .lock_save_irq()
        .remove(fd)
        .ok_or(KernelError::BadFd)?;
    match file {
        FileDescriptorEntryInner::Socket(socket) => {
            socket.lock().await.close().await?;
            Ok(0)
        }
        FileDescriptorEntryInner::OpenFile(file) => {
            if let Some(file) = Arc::into_inner(file) {
                let (ops, ctx) = &mut *file.lock().await;
                ops.release(ctx).await?;

                Ok(0)
            } else {
                Ok(0)
            }
        }
    }
}
