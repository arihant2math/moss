use super::fops::FileOps;
use crate::{
    process::fd_table::select::PollFlags,
    sync::{AsyncMutexGuard, Mutex},
};
use alloc::{boxed::Box, sync::Arc, vec::Vec};
use core::{future, pin::Pin, task::Poll};
use libkernel::{
    error::Result,
    fs::{Inode, OpenFlags, path::Path, pathbuf::PathBuf},
};

pub struct FileCtx {
    pub flags: OpenFlags,
    pub pos: u64,
}

impl FileCtx {
    pub fn new(flags: OpenFlags) -> Self {
        Self { flags, pos: 0 }
    }
}

pub struct OpenFile {
    inode: Option<Arc<dyn Inode>>,
    path: Option<PathBuf>,
    state: Mutex<(Box<dyn FileOps>, FileCtx)>,
}

impl OpenFile {
    pub fn new(ops: Box<dyn FileOps>, flags: OpenFlags) -> Self {
        Self {
            state: Mutex::new((ops, FileCtx::new(flags))),
            inode: None,
            path: None,
        }
    }

    pub fn update(&mut self, inode: Arc<dyn Inode>, path: PathBuf) {
        self.inode = Some(inode);
        self.path = Some(path);
    }

    pub fn inode(&self) -> Option<Arc<dyn Inode>> {
        self.inode.clone()
    }

    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    pub async fn flags(&self) -> OpenFlags {
        self.state.lock().await.1.flags
    }

    pub async fn set_flags(&self, flags: OpenFlags) {
        self.state.lock().await.1.flags = flags;
    }

    pub async fn lock(&self) -> AsyncMutexGuard<'_, (Box<dyn FileOps>, FileCtx)> {
        self.state.lock().await
    }

    pub async fn poll(
        &self,
        flags: PollFlags,
    ) -> impl Future<Output = Result<PollFlags>> + Send + use<> {
        let mut futs = Vec::new();

        {
            let (ops, _) = &mut *self.lock().await;

            if flags.contains(PollFlags::POLLIN) {
                let read_fut = ops.poll_read_ready();

                futs.push(
                    Box::pin(async move { read_fut.await.map(|_| PollFlags::POLLIN) })
                        as Pin<Box<dyn Future<Output = _> + Send>>,
                );
            }

            if flags.contains(PollFlags::POLLOUT) {
                let write_fut = ops.poll_write_ready();

                futs.push(Box::pin(async move {
                    write_fut.await.map(|_| PollFlags::POLLOUT)
                }));
            }
        }

        future::poll_fn(move |cx| {
            let mut flags = PollFlags::empty();

            // If no events were requested, return immediately.
            if futs.is_empty() {
                return Poll::Ready(Ok(PollFlags::empty()));
            }

            for fut in futs.iter_mut() {
                match fut.as_mut().poll(cx) {
                    Poll::Ready(Ok(flag)) => flags.insert(flag),
                    Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
                    Poll::Pending => continue,
                }
            }

            if flags.is_empty() {
                Poll::Pending
            } else {
                Poll::Ready(Ok(flags))
            }
        })
    }
}
