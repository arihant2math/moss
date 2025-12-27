use crate::{
    kernel::kpipe::KPipe,
    memory::uaccess::copy_to_user,
    process::{fd_table::Fd, thread_group::signal::SigId},
    sched::current_task,
    sync::CondVar,
};
use alloc::{boxed::Box, sync::Arc};
use async_trait::async_trait;
use core::{future, pin::pin, task::Poll};
use libkernel::{
    error::{KernelError, Result},
    fs::{OpenFlags, SeekFrom},
    memory::address::{TUA, UA},
    sync::condvar::WakeupType,
};
//
use super::{
    fops::FileOps,
    open_file::{FileCtx, OpenFile},
};

#[derive(Clone)]
struct PipeInner {
    buf: KPipe,
    other_side_gone: CondVar<bool>,
}

impl PipeInner {}

struct PipeReader {
    inner: PipeInner,
}

impl PipeReader {
    async fn do_read(&self, read_fut: impl Future<Output = Result<usize>>) -> Result<usize> {
        let mut read_fut = pin!(read_fut);
        let mut gone_fut =
            pin!(
                self.inner
                    .other_side_gone
                    .wait_until(|gone| if *gone { Some(()) } else { None })
            );

        future::poll_fn(move |cx| {
            // Check the consumption future first, before we check whether the
            // other side of the pipe has gone. This ensures we drain the buffer
            // first.
            if let Poll::Ready(r) = read_fut.as_mut().poll(cx) {
                Poll::Ready(r)
            } else if gone_fut.as_mut().poll(cx).is_ready() {
                Poll::Ready(Ok(0))
            } else {
                Poll::Pending
            }
        })
        .await
    }
}

#[async_trait]
impl FileOps for PipeReader {
    async fn read(&mut self, _ctx: &mut FileCtx, u_buf: UA, count: usize) -> Result<usize> {
        self.readat(u_buf, count, 0).await
    }

    async fn readat(&mut self, u_buf: UA, count: usize, _offset: u64) -> Result<usize> {
        if count == 0 {
            return Ok(0);
        }

        self.do_read(self.inner.buf.copy_to_user(u_buf, count))
            .await
    }

    async fn write(&mut self, _ctx: &mut FileCtx, _buf: UA, _count: usize) -> Result<usize> {
        Err(KernelError::BadFd)
    }

    async fn writeat(&mut self, _buf: UA, _count: usize, _offset: u64) -> Result<usize> {
        Err(KernelError::BadFd)
    }

    async fn seek(&mut self, _ctx: &mut FileCtx, _pos: SeekFrom) -> Result<u64> {
        Err(KernelError::SeekPipe)
    }

    async fn splice_into(
        &mut self,
        _ctx: &mut FileCtx,
        kbuf: &KPipe,
        count: usize,
    ) -> Result<usize> {
        self.do_read(async { Ok(kbuf.splice_from(&self.inner.buf, count).await) })
            .await
    }
}

impl Drop for PipeReader {
    fn drop(&mut self) {
        // notify any writers that the read end of the pipe has gone.
        self.inner.other_side_gone.update(|gone| {
            *gone = true;
            WakeupType::All
        });
    }
}

struct PipeWriter {
    inner: PipeInner,
}

impl PipeWriter {
    async fn do_write(&self, write_fut: impl Future<Output = Result<usize>>) -> Result<usize> {
        let mut write_fut = pin!(write_fut);
        let mut gone_fut =
            pin!(
                self.inner
                    .other_side_gone
                    .wait_until(|gone| if *gone { Some(()) } else { None })
            );

        future::poll_fn(move |cx| {
            // Check the gone future first, before we write data into the
            // buffer. There's no point writing data if there's no consumer!
            if gone_fut.as_mut().poll(cx).is_ready() {
                // Other side of the pipe has been closed.
                current_task().raise_task_signal(SigId::SIGPIPE);
                Poll::Ready(Err(KernelError::BrokenPipe))
            } else if let Poll::Ready(x) = write_fut.as_mut().poll(cx) {
                Poll::Ready(x)
            } else {
                Poll::Pending
            }
        })
        .await
    }
}

#[async_trait]
impl FileOps for PipeWriter {
    async fn read(&mut self, _ctx: &mut FileCtx, _buf: UA, _count: usize) -> Result<usize> {
        Err(KernelError::BadFd)
    }

    async fn readat(&mut self, _buf: UA, _count: usize, _offset: u64) -> Result<usize> {
        Err(KernelError::BadFd)
    }

    async fn write(&mut self, _ctx: &mut FileCtx, u_buf: UA, count: usize) -> Result<usize> {
        self.writeat(u_buf, count, 0).await
    }

    async fn writeat(&mut self, u_buf: UA, count: usize, _offset: u64) -> Result<usize> {
        if count == 0 {
            return Ok(0);
        }

        self.do_write(self.inner.buf.copy_from_user(u_buf, count))
            .await
    }

    async fn seek(&mut self, _ctx: &mut FileCtx, _pos: SeekFrom) -> Result<u64> {
        Err(KernelError::SeekPipe)
    }

    async fn splice_from(
        &mut self,
        _ctx: &mut FileCtx,
        kbuf: &KPipe,
        count: usize,
    ) -> Result<usize> {
        self.do_write(async { Ok(self.inner.buf.splice_from(kbuf, count).await) })
            .await
    }
}

impl Drop for PipeWriter {
    fn drop(&mut self) {
        // notify any readers that the write end of the pipe has gone.
        self.inner.other_side_gone.update(|gone| {
            *gone = true;
            WakeupType::All
        });
    }
}

pub async fn sys_pipe2(fds: TUA<[Fd; 2]>, flags: u32) -> Result<usize> {
    let flags = OpenFlags::from_bits_retain(flags);

    let kbuf = KPipe::new()?;
    let condvar = CondVar::new(false);

    let inner = PipeInner {
        buf: kbuf,
        other_side_gone: condvar,
    };

    let reader = PipeReader {
        inner: inner.clone(),
    };

    let writer = PipeWriter { inner };

    let (read_fd, write_fd) = {
        let task = current_task();
        let mut fds = task.fd_table.lock_save_irq();

        let read_file = OpenFile::new(Box::new(reader), flags);
        let write_file = OpenFile::new(Box::new(writer), flags);

        let read_fd = fds.insert(Arc::new(read_file))?;
        let write_fd = fds.insert(Arc::new(write_file))?;

        (read_fd, write_fd)
    };

    // TODO: What if the copy fails here, we've leaked the above file
    // descriptors.
    copy_to_user(fds, [read_fd as _, write_fd as _]).await?;

    Ok(0)
}
