use alloc::{boxed::Box, vec::Vec};
use core::{
    future::{Future, poll_fn},
    iter,
    pin::{Pin, pin},
    task::Poll,
};
use libkernel::{
    error::{KernelError, Result},
    memory::address::TUA,
};

use super::Fd;
use crate::{
    clock::timespec::TimeSpec,
    drivers::timer::sleep,
    memory::uaccess::{
        UserCopyable, copy_from_user, copy_obj_array_from_user, copy_objs_to_user, copy_to_user,
    },
    process::thread_group::signal::SigSet,
    sched::syscall_ctx::ProcessCtx,
};

const SET_SIZE: usize = 1024;

#[derive(Clone, Copy, Debug)]
pub struct FdSet {
    set: [u64; SET_SIZE / (8 * core::mem::size_of::<u64>())],
}

impl FdSet {
    fn iter_fds(&self, max: usize) -> impl Iterator<Item = Fd> {
        let mut candidate_fd = 0usize;

        iter::from_fn(move || {
            loop {
                if candidate_fd == max {
                    return None;
                }

                if self.set[candidate_fd / 64] & (1 << (candidate_fd % 64)) != 0 {
                    let ret = Fd(candidate_fd as i32);
                    candidate_fd += 1;

                    return Some(ret);
                }

                candidate_fd += 1;
            }
        })
    }

    fn zero(&mut self) {
        self.set = [0; _];
    }

    fn set_fd(&mut self, fd: Fd) {
        let fd = fd.as_raw();

        self.set[fd as usize / 64] |= 1 << (fd % 64);
    }
}

unsafe impl UserCopyable for FdSet {}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub(crate) struct PSelect6SigMask {
    sigmask: TUA<SigSet>,
    sigsetsize: usize,
}

unsafe impl UserCopyable for PSelect6SigMask {}

// TODO: handle exceptfds readiness semantics.
pub async fn sys_pselect6(
    ctx: &ProcessCtx,
    max: i32,
    readfds: TUA<FdSet>,
    writefds: TUA<FdSet>,
    exceptfds: TUA<FdSet>,
    timeout: TUA<TimeSpec>,
    mask: TUA<PSelect6SigMask>,
) -> Result<usize> {
    if max < 0 || max as usize > SET_SIZE {
        return Err(KernelError::InvalidValue);
    }

    let max = max as usize;
    let task = ctx.shared();

    let mut read_fd_set = if readfds.is_null() {
        None
    } else {
        Some(copy_from_user(readfds).await?)
    };

    let mut read_fds = Vec::new();

    let mut write_fd_set = if writefds.is_null() {
        None
    } else {
        Some(copy_from_user(writefds).await?)
    };

    let mut write_fds = Vec::new();

    let mut except_fd_set = if exceptfds.is_null() {
        None
    } else {
        Some(copy_from_user(exceptfds).await?)
    };

    let mut timeout_fut = if timeout.is_null() {
        None
    } else {
        let duration = copy_from_user(timeout).await?.into();
        Some(pin!(sleep(duration)))
    };

    if let Some(ref read_fd_set) = read_fd_set {
        for fd in read_fd_set.iter_fds(max) {
            let file = task
                .fd_table
                .lock_save_irq()
                .get(fd)
                .ok_or(KernelError::BadFd)?;

            read_fds.push((
                Box::pin(async move {
                    let (ops, _) = &mut *file.lock().await;

                    ops.poll_read_ready().await
                }),
                fd,
            ));
        }
    }

    if let Some(ref write_fd_set) = write_fd_set {
        for fd in write_fd_set.iter_fds(max) {
            let file = task
                .fd_table
                .lock_save_irq()
                .get(fd)
                .ok_or(KernelError::BadFd)?;

            write_fds.push((
                Box::pin(async move {
                    let (ops, _) = &mut *file.lock().await;

                    ops.poll_write_ready().await
                }),
                fd,
            ));
        }
    }

    let mask = if mask.is_null() {
        None
    } else {
        let args: PSelect6SigMask = copy_from_user(mask).await?;

        if args.sigsetsize != core::mem::size_of::<SigSet>() {
            return Err(KernelError::InvalidValue);
        }

        if args.sigmask.is_null() {
            None
        } else {
            Some(copy_from_user(args.sigmask).await?)
        }
    };
    let old_sigmask = task.sig_mask.load();
    if let Some(mask) = mask {
        let mut new_sigmask = mask;
        new_sigmask.remove(SigSet::UNMASKABLE_SIGNALS);
        task.sig_mask.store(new_sigmask);
    }

    if let Some(ref mut read_fd_set) = read_fd_set {
        read_fd_set.zero();
    }
    if let Some(ref mut write_fd_set) = write_fd_set {
        write_fd_set.zero();
    }
    if let Some(ref mut except_fd_set) = except_fd_set {
        except_fd_set.zero();
    }

    let n = poll_fn(|cx| {
        let mut num_ready: usize = 0;

        for (fut, fd) in read_fds.iter_mut() {
            if fut.as_mut().poll(cx).is_ready() {
                // Mark the is_ready bool. Don't break out of the loop just
                // yet, we may as well check all fds while we're here.
                if let Some(ref mut read_fd_set) = read_fd_set {
                    read_fd_set.set_fd(*fd);
                }
                num_ready += 1;
            }
        }

        for (fut, fd) in write_fds.iter_mut() {
            if fut.as_mut().poll(cx).is_ready() {
                if let Some(ref mut write_fd_set) = write_fd_set {
                    write_fd_set.set_fd(*fd);
                }
                num_ready += 1;
            }
        }

        if num_ready == 0 {
            // Check if done
            if let Some(ref mut timeout) = timeout_fut {
                timeout.as_mut().poll(cx).map(|_| 0)
            } else {
                Poll::Pending
            }
        } else {
            Poll::Ready(num_ready)
        }
    })
    .await;

    let readfds_copy_result = if let Some(read_fd_set) = read_fd_set {
        copy_to_user(readfds, read_fd_set).await
    } else {
        Ok(())
    };
    let writefds_copy_result = if let Some(write_fd_set) = write_fd_set {
        copy_to_user(writefds, write_fd_set).await
    } else {
        Ok(())
    };
    let exceptfds_copy_result = if let Some(except_fd_set) = except_fd_set {
        copy_to_user(exceptfds, except_fd_set).await
    } else {
        Ok(())
    };

    if mask.is_some() {
        task.sig_mask.store(old_sigmask);
    }

    readfds_copy_result?;
    writefds_copy_result?;
    exceptfds_copy_result?;

    Ok(n)
}

bitflags::bitflags! {
    #[derive(Clone, Copy, Debug)]
    pub struct PollFlags: i16 {
        const POLLIN     = 0x001; // Read
        const POLLPRI    = 0x002; // Priority ready (mainly sockets)
        const POLLOUT    = 0x004; // Write
        const POLLERR    = 0x008; // Any errors.
        const POLLHUP    = 0x010; // Hangup
        const POLLNVAL   = 0x020;
        const POLLRDNORM = 0x040;
        const POLLRDBAND = 0x080;
        const POLLWRNORM = 0x100;
        const POLLWRBAND = 0x200;
        const POLLMSG    = 0x400;
    }
}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct PollFd {
    fd: Fd,
    events: PollFlags,
    revents: PollFlags,
}

unsafe impl UserCopyable for PollFd {}

pub async fn sys_ppoll(
    ctx: &ProcessCtx,
    ufds: TUA<PollFd>,
    nfds: u32,
    timeout: TUA<TimeSpec>,
    sigmask: TUA<SigSet>,
    sigset_len: usize,
) -> Result<usize> {
    struct PendingPoll {
        idx: usize,
        requested: PollFlags,
        fut: Pin<Box<dyn Future<Output = Result<PollFlags>> + Send>>,
    }

    fn requested_poll_mask(events: PollFlags) -> PollFlags {
        let mut mask = PollFlags::empty();

        if events.intersects(PollFlags::POLLIN | PollFlags::POLLRDNORM) {
            mask.insert(PollFlags::POLLIN);
        }

        if events.intersects(PollFlags::POLLOUT | PollFlags::POLLWRNORM) {
            mask.insert(PollFlags::POLLOUT);
        }

        mask
    }

    fn returned_poll_mask(requested: PollFlags, ready: PollFlags) -> PollFlags {
        let mut revents = ready
            & !(PollFlags::POLLIN
                | PollFlags::POLLRDNORM
                | PollFlags::POLLOUT
                | PollFlags::POLLWRNORM);

        if ready.contains(PollFlags::POLLIN) {
            if requested.contains(PollFlags::POLLIN) {
                revents.insert(PollFlags::POLLIN);
            }
            if requested.contains(PollFlags::POLLRDNORM) {
                revents.insert(PollFlags::POLLRDNORM);
            }
            if !requested.intersects(PollFlags::POLLIN | PollFlags::POLLRDNORM) {
                revents.insert(PollFlags::POLLIN);
            }
        }

        if ready.contains(PollFlags::POLLOUT) {
            if requested.contains(PollFlags::POLLOUT) {
                revents.insert(PollFlags::POLLOUT);
            }
            if requested.contains(PollFlags::POLLWRNORM) {
                revents.insert(PollFlags::POLLWRNORM);
            }
            if !requested.intersects(PollFlags::POLLOUT | PollFlags::POLLWRNORM) {
                revents.insert(PollFlags::POLLOUT);
            }
        }

        revents
    }

    let task = ctx.shared();

    let mask = if sigmask.is_null() {
        None
    } else {
        if sigset_len != core::mem::size_of::<SigSet>() {
            return Err(KernelError::InvalidValue);
        }

        Some(copy_from_user(sigmask).await?)
    };
    let has_mask = mask.is_some();

    let mut poll_fds = copy_obj_array_from_user(ufds, nfds as _).await?;
    for poll_fd in &mut poll_fds {
        poll_fd.revents = PollFlags::empty();
    }

    let mut timeout_fut = if timeout.is_null() {
        None
    } else {
        let duration = TimeSpec::copy_from_user(timeout).await?.into();
        Some(pin!(sleep(duration)))
    };

    let mut futs = Vec::<PendingPoll>::new();

    for (idx, poll_fd) in poll_fds.iter_mut().enumerate() {
        let fd = poll_fd.fd;
        let events = poll_fd.events;

        if fd.as_raw() < 0 {
            continue;
        }

        let Some(open_file) = task.fd_table.lock_save_irq().get(fd) else {
            poll_fd.revents = PollFlags::POLLNVAL;
            continue;
        };

        let wait_mask = requested_poll_mask(events);
        if wait_mask.is_empty() {
            continue;
        }

        let poll_fut = open_file.poll(wait_mask).await;
        futs.push(PendingPoll {
            idx,
            requested: events,
            fut: Box::pin(poll_fut) as Pin<Box<dyn Future<Output = Result<PollFlags>> + Send>>,
        });
    }

    let immediately_ready = poll_fds
        .iter()
        .filter(|poll_fd| !poll_fd.revents.is_empty())
        .count();

    let old_sigmask = task.sig_mask.load();
    if let Some(mut mask) = mask {
        mask.remove(SigSet::UNMASKABLE_SIGNALS);
        task.sig_mask.store(mask);
    }

    let num_ready_result = poll_fn(|cx| {
        let mut num_ready = immediately_ready;

        for pending in futs.iter_mut() {
            match pending.fut.as_mut().poll(cx) {
                Poll::Ready(Ok(revents)) => {
                    let revents = returned_poll_mask(pending.requested, revents);
                    poll_fds[pending.idx].revents = revents;
                    if !revents.is_empty() {
                        num_ready += 1;
                    }
                }
                Poll::Ready(Err(e)) => return Poll::Ready(Err::<_, KernelError>(e)),
                Poll::Pending => continue,
            }
        }

        if num_ready == 0 {
            if let Some(ref mut timeout) = timeout_fut {
                timeout.as_mut().poll(cx).map(|_| Ok(0))
            } else {
                Poll::Pending
            }
        } else {
            Poll::Ready(Ok(num_ready))
        }
    })
    .await;

    drop(futs);

    if has_mask {
        task.sig_mask.store(old_sigmask);
    }

    let num_ready = num_ready_result?;
    copy_objs_to_user(&poll_fds, ufds).await?;

    Ok(num_ready)
}
