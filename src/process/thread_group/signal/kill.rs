use crate::{
    clock::timespec::TimeSpec,
    drivers::timer::sleep,
    memory::uaccess::{copy_from_user, copy_to_user},
    process::{
        Tid,
        thread_group::{Pgid, Tgid, ThreadGroup, pid::PidT},
    },
    sched::{current_work, sched_task::Work, syscall_ctx::ProcessCtx, waker::create_waker},
};

use super::{KSigInfo, PendingSignal, SigId, SigSet, uaccess::UserSigId};
use crate::process::thread_group::TG_LIST;
use alloc::{boxed::Box, sync::Arc};
use core::{
    future::Future,
    pin::Pin,
    task::{Context, Poll},
};
use futures::FutureExt;
use libkernel::{
    error::{KernelError, Result},
    memory::address::TUA,
};

pub fn sys_kill(ctx: &ProcessCtx, pid: PidT, signal: UserSigId) -> Result<usize> {
    let signal: SigId = signal.try_into()?;

    let current_task = ctx.shared();
    // Kill ourselves
    if pid == current_task.process.tgid.value() as PidT {
        current_task.process.deliver_signal(signal);

        return Ok(0);
    }

    match pid {
        p if p > 0 => {
            let target_tg = ThreadGroup::get(Tgid(p as _)).ok_or(KernelError::NoProcess)?;
            target_tg.deliver_signal(signal);
        }

        0 => {
            let our_pgid = *current_task.process.pgid.lock_save_irq();
            // Iterate over all thread groups and signal the ones that are in
            // the same PGID.
            for tg_weak in crate::process::thread_group::TG_LIST
                .lock_save_irq()
                .values()
            {
                if let Some(tg) = tg_weak.upgrade()
                    && *tg.pgid.lock_save_irq() == our_pgid
                {
                    tg.deliver_signal(signal);
                }
            }
        }

        p if p < 0 && p != -1 => {
            let target_pgid = Pgid((-p) as _);
            for tg_weak in crate::process::thread_group::TG_LIST
                .lock_save_irq()
                .values()
            {
                if let Some(tg) = tg_weak.upgrade()
                    && *tg.pgid.lock_save_irq() == target_pgid
                {
                    tg.deliver_signal(signal);
                }
            }
        }

        _ => return Err(KernelError::NotSupported),
    }

    Ok(0)
}

pub fn sys_tkill(ctx: &ProcessCtx, tid: PidT, signal: UserSigId) -> Result<usize> {
    let target_tid = Tid(tid as _);
    let current_task = ctx.shared();

    let signal: SigId = signal.try_into()?;

    // The fast-path case.
    if current_task.tid == target_tid {
        current_task.raise_task_signal(signal);
    } else {
        let task = current_task
            .process
            .tasks
            .lock_save_irq()
            .get(&target_tid)
            .and_then(|t| t.upgrade())
            .ok_or(KernelError::NoProcess)?;

        task.raise_task_signal(signal);
        create_waker(task).wake();
    }

    Ok(0)
}

pub fn sys_tgkill(_ctx: &ProcessCtx, tgid: PidT, tid: PidT, signal: UserSigId) -> Result<usize> {
    if tgid <= 0 || tid <= 0 {
        return Err(KernelError::InvalidValue);
    }

    let signal: SigId = signal.try_into()?;
    let target_tg = ThreadGroup::get(Tgid(tgid as _)).ok_or(KernelError::NoProcess)?;
    let target_tid = Tid(tid as _);

    let task = target_tg
        .tasks
        .lock_save_irq()
        .get(&target_tid)
        .and_then(|t| t.upgrade())
        .ok_or(KernelError::NoProcess)?;

    task.raise_task_signal(signal);
    create_waker(task).wake();
    Ok(0)
}

pub async fn sys_rt_sigpending(set: TUA<SigSet>, sigsetsize: usize) -> Result<usize> {
    if sigsetsize != size_of::<SigSet>() {
        return Err(KernelError::InvalidValue);
    }

    let task = current_work();
    let pending = task.pending_signals.lock_save_irq().set()
        | task.process.pending_signals.lock_save_irq().set();
    let blocked_pending = pending.intersection(task.sig_mask.load());
    copy_to_user(set, blocked_pending).await?;
    Ok(0)
}

pub async fn sys_rt_sigqueueinfo(
    _ctx: &ProcessCtx,
    pid: PidT,
    sig: UserSigId,
    uinfo: TUA<KSigInfo>,
) -> Result<usize> {
    if pid <= 0 {
        return Err(KernelError::InvalidValue);
    }

    let sig: SigId = sig.try_into()?;
    let info = copy_from_user(uinfo).await?;
    let target_tg = ThreadGroup::get(Tgid(pid as _)).ok_or(KernelError::NoProcess)?;
    target_tg.deliver_pending_signal(PendingSignal::with_info(sig, info));
    Ok(0)
}

pub async fn sys_rt_tgsigqueueinfo(
    _ctx: &ProcessCtx,
    tgid: PidT,
    tid: PidT,
    sig: UserSigId,
    uinfo: TUA<KSigInfo>,
) -> Result<usize> {
    if tgid <= 0 || tid <= 0 {
        return Err(KernelError::InvalidValue);
    }

    let sig: SigId = sig.try_into()?;
    let info = copy_from_user(uinfo).await?;
    let target_tg = ThreadGroup::get(Tgid(tgid as _)).ok_or(KernelError::NoProcess)?;
    let target_tid = Tid(tid as _);

    let task = target_tg
        .tasks
        .lock_save_irq()
        .get(&target_tid)
        .and_then(|t| t.upgrade())
        .ok_or(KernelError::NoProcess)?;

    task.queue_task_signal(PendingSignal::with_info(sig, info));
    create_waker(task).wake();
    Ok(0)
}

pub async fn sys_rt_sigtimedwait(
    set: TUA<SigSet>,
    info: TUA<KSigInfo>,
    timeout: TUA<TimeSpec>,
    sigsetsize: usize,
) -> Result<usize> {
    if sigsetsize != size_of::<SigSet>() {
        return Err(KernelError::InvalidValue);
    }

    let mut wanted = copy_from_user(set).await?;
    wanted.remove(SigSet::UNMASKABLE_SIGNALS);
    let blocked = SigSet::from_bits_retain(!wanted.bits());

    if let Some(signal) = take_waited_signal(blocked) {
        return finish_sigtimedwait(signal, info).await;
    }

    if timeout.is_null() {
        loop {
            SignalSetWait::new(current_work(), blocked).await;
            if let Some(signal) = take_waited_signal(blocked) {
                return finish_sigtimedwait(signal, info).await;
            }
        }
    }

    let duration = TimeSpec::copy_from_user(timeout).await?.into();
    let mut wait = SignalSetWait::new(current_work(), blocked).fuse();
    let mut timeout = Box::pin(sleep(duration).fuse());

    futures::select_biased! {
        _ = wait => {
            match take_waited_signal(blocked) {
                Some(signal) => finish_sigtimedwait(signal, info).await,
                None => Err(KernelError::TryAgain),
            }
        },
        _ = timeout => Err(KernelError::TryAgain),
    }
}

fn take_waited_signal(blocked: SigSet) -> Option<PendingSignal> {
    let task = current_work();
    task.pending_signals
        .lock_save_irq()
        .take_signal(blocked)
        .or_else(|| {
            task.process
                .pending_signals
                .lock_save_irq()
                .take_signal(blocked)
        })
}

async fn finish_sigtimedwait(signal: PendingSignal, info: TUA<KSigInfo>) -> Result<usize> {
    if !info.is_null() {
        copy_to_user(info, signal.info).await?;
    }
    Ok(signal.id.user_id() as usize)
}

struct SignalSetWait {
    task: Arc<Work>,
    blocked: SigSet,
    token: Option<u64>,
}

impl SignalSetWait {
    fn new(task: Arc<Work>, blocked: SigSet) -> Self {
        Self {
            task,
            blocked,
            token: None,
        }
    }
}

impl Drop for SignalSetWait {
    fn drop(&mut self) {
        if let Some(token) = self.token.take() {
            self.task.signal_notifier.lock_save_irq().remove(token);
        }
    }
}

impl Future for SignalSetWait {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = unsafe { self.as_mut().get_unchecked_mut() };
        let mut notifier = this.task.signal_notifier.lock_save_irq();

        let ready = this
            .task
            .pending_signals
            .lock_save_irq()
            .peek_signal(this.blocked)
            .is_some()
            || this
                .task
                .process
                .pending_signals
                .lock_save_irq()
                .peek_signal(this.blocked)
                .is_some();

        if ready {
            if let Some(token) = this.token.take() {
                notifier.remove(token);
            }
            return Poll::Ready(());
        }

        if let Some(token) = this.token.take() {
            notifier.remove(token);
        }
        this.token = Some(notifier.register(cx.waker()));

        Poll::Pending
    }
}

pub fn send_signal_to_pg(pgid: Pgid, signal: SigId) {
    for tg_weak in TG_LIST.lock_save_irq().values() {
        if let Some(tg) = tg_weak.upgrade()
            && *tg.pgid.lock_save_irq() == pgid
        {
            tg.deliver_signal(signal);
        }
    }
}
