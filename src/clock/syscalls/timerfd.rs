use crate::{
    clock::{
        ClockId,
        realtime::{self, date},
        timer::{TimerNamespace, make_timer_id},
        timespec::TimeSpec,
    },
    drivers::timer::{Instant, SYS_TIMER, now, uptime},
    fs::{
        fops::FileOps,
        open_file::{FileCtx, OpenFile},
    },
    memory::uaccess::{UserCopyable, copy_from_user, copy_to_user},
    process::{
        Tid,
        fd_table::{Fd, FdFlags},
    },
    sched::syscall_ctx::ProcessCtx,
    sync::SpinLock,
};
use alloc::{boxed::Box, sync::Arc};
use core::{
    future::Future,
    mem::size_of,
    pin::Pin,
    sync::atomic::{AtomicU32, Ordering},
    task::{Context, Poll},
    time::Duration,
};
use libkernel::{
    error::{KernelError, Result},
    fs::OpenFlags,
    memory::address::{TUA, UA},
    sync::waker_set::WakerSet,
};

const TFD_TIMER_ABSTIME: i32 = 1;
const TFD_TIMER_CANCEL_ON_SET: i32 = 2;
const TFD_CREATE_FLAGS: i32 =
    OpenFlags::O_NONBLOCK.bits() as i32 | OpenFlags::O_CLOEXEC.bits() as i32;
const TFD_SETTIME_FLAGS: i32 = TFD_TIMER_ABSTIME | TFD_TIMER_CANCEL_ON_SET;

static NEXT_TIMERFD_ID: AtomicU32 = AtomicU32::new(1);

#[repr(C)]
#[derive(Clone, Copy)]
pub struct ITimerSpec {
    pub it_interval: TimeSpec,
    pub it_value: TimeSpec,
}

impl Default for ITimerSpec {
    fn default() -> Self {
        Self {
            it_interval: TimeSpec {
                tv_sec: 0,
                tv_nsec: 0,
            },
            it_value: TimeSpec {
                tv_sec: 0,
                tv_nsec: 0,
            },
        }
    }
}

unsafe impl UserCopyable for ITimerSpec {}

impl ITimerSpec {
    fn validate(&self) -> Result<()> {
        validate_timespec(self.it_interval)?;
        validate_timespec(self.it_value)?;
        Ok(())
    }

    fn is_disarmed(&self) -> bool {
        self.it_value.tv_sec == 0 && self.it_value.tv_nsec == 0
    }

    fn interval(&self) -> Option<Duration> {
        if self.it_interval.tv_sec == 0 && self.it_interval.tv_nsec == 0 {
            None
        } else {
            Some(self.it_interval.into())
        }
    }
}

fn validate_timespec(value: TimeSpec) -> Result<()> {
    if value.tv_sec < 0 || value.tv_nsec >= 1_000_000_000 {
        Err(KernelError::InvalidValue)
    } else {
        Ok(())
    }
}

fn duration_to_nanos(duration: Duration) -> u128 {
    duration.as_secs() as u128 * 1_000_000_000 + duration.subsec_nanos() as u128
}

fn duration_from_nanos_saturating(nanos: u128) -> Duration {
    let secs = (nanos / 1_000_000_000).min(u64::MAX as u128) as u64;
    let subsec = (nanos % 1_000_000_000) as u32;
    Duration::new(secs, subsec)
}

fn duration_add_saturating(lhs: Duration, rhs: Duration) -> Duration {
    duration_from_nanos_saturating(duration_to_nanos(lhs).saturating_add(duration_to_nanos(rhs)))
}

fn duration_mul_saturating(duration: Duration, factor: u64) -> Duration {
    duration_from_nanos_saturating(duration_to_nanos(duration).saturating_mul(factor as u128))
}

fn duration_div_floor(lhs: Duration, rhs: Duration) -> u64 {
    let rhs = duration_to_nanos(rhs);
    if let Some(div) = duration_to_nanos(lhs).checked_div(rhs) {
        div.min(u64::MAX as u128) as u64
    } else {
        0
    }
}

#[derive(Clone, Copy)]
enum Deadline {
    Monotonic(Duration),
    Realtime(Duration),
}

impl Deadline {
    fn value(self) -> Duration {
        match self {
            Self::Monotonic(value) | Self::Realtime(value) => value,
        }
    }

    fn set_value(&mut self, new_value: Duration) {
        match self {
            Self::Monotonic(value) | Self::Realtime(value) => *value = new_value,
        }
    }

    fn is_realtime(self) -> bool {
        matches!(self, Self::Realtime(_))
    }

    fn current(self) -> Duration {
        match self {
            Self::Monotonic(_) => uptime(),
            Self::Realtime(_) => date(),
        }
    }
}

#[derive(Clone, Copy)]
struct ArmedTimer {
    deadline: Deadline,
    interval: Option<Duration>,
    cancel_on_set: bool,
    realtime_seq: u64,
}

#[derive(Default)]
struct TimerFdState {
    armed: Option<ArmedTimer>,
    expirations: u64,
    canceled: bool,
    waiters: WakerSet,
    realtime_listener: Option<u64>,
}

impl TimerFdState {
    fn is_read_ready(&self) -> bool {
        self.canceled || self.expirations > 0
    }
}

pub struct TimerFd {
    inner: Arc<TimerFdInner>,
}

impl TimerFd {
    fn new(clock_id: ClockId, owner_tid: Tid) -> Self {
        Self {
            inner: Arc::new(TimerFdInner {
                id: NEXT_TIMERFD_ID.fetch_add(1, Ordering::Relaxed),
                owner_tid,
                clock_id,
                state: SpinLock::new(TimerFdState::default()),
            }),
        }
    }

    fn inner(&self) -> &Arc<TimerFdInner> {
        &self.inner
    }

    async fn read_impl(&self, buf: UA, count: usize, nonblock: bool) -> Result<usize> {
        if count < size_of::<u64>() {
            return Err(KernelError::InvalidValue);
        }

        enum ReadOutcome {
            Canceled,
            Expirations(u64),
            Retry,
            Wait,
        }

        loop {
            let outcome = {
                let mut state = self.inner.state.lock_save_irq();
                self.inner.sync_state_locked(&mut state);

                if state.canceled {
                    state.canceled = false;
                    ReadOutcome::Canceled
                } else if state.expirations > 0 {
                    let expirations = state.expirations;
                    state.expirations = 0;
                    ReadOutcome::Expirations(expirations)
                } else if nonblock {
                    ReadOutcome::Retry
                } else {
                    ReadOutcome::Wait
                }
            };

            match outcome {
                ReadOutcome::Canceled => return Err(KernelError::Canceled),
                ReadOutcome::Expirations(expirations) => {
                    copy_to_user(TUA::from_value(buf.value()), expirations).await?;
                    return Ok(size_of::<u64>());
                }
                ReadOutcome::Retry => return Err(KernelError::TryAgain),
                ReadOutcome::Wait => TimerFdWait::new(self.inner.clone()).await,
            }
        }
    }
}

struct SetTimeOutcome {
    old_value: ITimerSpec,
    canceled: bool,
}

struct TimerFdInner {
    id: u32,
    owner_tid: Tid,
    clock_id: ClockId,
    state: SpinLock<TimerFdState>,
}

impl TimerFdInner {
    fn timer_id(&self) -> u64 {
        make_timer_id(TimerNamespace::TimerFd, self.id)
    }

    fn sync_state_locked(&self, state: &mut TimerFdState) {
        let Some(mut armed) = state.armed else {
            return;
        };

        if armed.deadline.is_realtime() {
            let seq = realtime::discontinuity_seq();
            if armed.realtime_seq != seq {
                armed.realtime_seq = seq;
                if armed.cancel_on_set {
                    state.canceled = true;
                }
            }
        }

        let now = armed.deadline.current();
        let deadline = armed.deadline.value();
        if now < deadline {
            state.armed = Some(armed);
            return;
        }

        let expirations = if let Some(interval) = armed.interval {
            1u64.saturating_add(duration_div_floor(now.saturating_sub(deadline), interval))
        } else {
            1
        };

        state.expirations = state.expirations.saturating_add(expirations);

        if let Some(interval) = armed.interval {
            armed.deadline.set_value(duration_add_saturating(
                deadline,
                duration_mul_saturating(interval, expirations),
            ));
            state.armed = Some(armed);
        } else {
            state.armed = None;
        }
    }

    fn curr_value_locked(&self, state: &mut TimerFdState) -> ITimerSpec {
        self.sync_state_locked(state);

        let Some(armed) = state.armed else {
            return ITimerSpec::default();
        };

        let current = armed.deadline.current();
        let remaining = armed.deadline.value().saturating_sub(current);
        ITimerSpec {
            it_interval: armed.interval.unwrap_or_default().into(),
            it_value: remaining.into(),
        }
    }

    fn next_wakeup_locked(&self, state: &TimerFdState) -> Option<Instant> {
        let armed = state.armed?;
        match armed.deadline {
            Deadline::Monotonic(target) => {
                let remaining = target.saturating_sub(uptime());
                now().map(|now| now + remaining)
            }
            Deadline::Realtime(target) => realtime::monotonic_deadline_for(target),
        }
    }

    fn make_armed_timer(&self, flags: i32, spec: ITimerSpec) -> Result<Option<ArmedTimer>> {
        if spec.is_disarmed() {
            return Ok(None);
        }

        let interval = spec.interval();
        let value: Duration = spec.it_value.into();
        let absolute = flags & TFD_TIMER_ABSTIME != 0;
        let cancel_on_set = flags & TFD_TIMER_CANCEL_ON_SET != 0;

        if cancel_on_set && (!absolute || self.clock_id != ClockId::Realtime) {
            return Err(KernelError::InvalidValue);
        }

        let deadline = match self.clock_id {
            ClockId::Monotonic => {
                if absolute {
                    Deadline::Monotonic(value)
                } else {
                    Deadline::Monotonic(duration_add_saturating(uptime(), value))
                }
            }
            ClockId::Realtime => {
                if absolute {
                    Deadline::Realtime(value)
                } else {
                    Deadline::Monotonic(duration_add_saturating(uptime(), value))
                }
            }
            _ => return Err(KernelError::InvalidValue),
        };

        Ok(Some(ArmedTimer {
            deadline,
            interval,
            cancel_on_set,
            realtime_seq: realtime::discontinuity_seq(),
        }))
    }

    fn register_realtime_listener_locked(self: &Arc<Self>, state: &mut TimerFdState) {
        if state.realtime_listener.is_some() {
            return;
        }

        let weak_self = Arc::downgrade(self);
        let listener = Arc::new(move |seq| {
            if let Some(inner) = weak_self.upgrade() {
                inner.on_realtime_clock_change(seq);
            }
        });
        state.realtime_listener = Some(realtime::register_change_listener(listener));
    }

    fn unregister_realtime_listener_locked(&self, state: &mut TimerFdState) {
        if let Some(listener) = state.realtime_listener.take() {
            realtime::unregister_change_listener(listener);
        }
    }

    fn rearm_scheduled_timer(self: &Arc<Self>) {
        if let Some(timer) = SYS_TIMER.get() {
            timer.remove_scheduled_timer(self.owner_tid, self.timer_id());

            let next = {
                let state = self.state.lock_save_irq();
                self.next_wakeup_locked(&state)
            };

            if let Some(when) = next {
                let weak_self = Arc::downgrade(self);
                let callback = Box::new(move |_tid: Tid, _id: u64| {
                    weak_self.upgrade().and_then(|inner| inner.on_timer_irq())
                });
                timer.schedule_timer(self.owner_tid, self.timer_id(), callback, when);
            }
        }
    }

    fn settime(self: &Arc<Self>, flags: i32, spec: ITimerSpec) -> Result<SetTimeOutcome> {
        let new_armed = self.make_armed_timer(flags, spec)?;

        let mut state = self.state.lock_save_irq();
        let old_value = self.curr_value_locked(&mut state);
        let canceled = state.canceled;

        self.unregister_realtime_listener_locked(&mut state);
        state.armed = None;
        state.canceled = false;
        state.expirations = 0;

        state.armed = new_armed;

        let needs_realtime_listener = state
            .armed
            .is_some_and(|armed| matches!(armed.deadline, Deadline::Realtime(_)));
        if needs_realtime_listener {
            self.register_realtime_listener_locked(&mut state);
        }

        self.sync_state_locked(&mut state);
        if state.is_read_ready() {
            state.waiters.wake_all();
        }
        drop(state);

        self.rearm_scheduled_timer();

        Ok(SetTimeOutcome {
            old_value,
            canceled,
        })
    }

    fn on_timer_irq(self: Arc<Self>) -> Option<Instant> {
        let mut state = self.state.lock_save_irq();
        self.sync_state_locked(&mut state);
        if state.is_read_ready() {
            state.waiters.wake_all();
        }
        self.next_wakeup_locked(&state)
    }

    fn on_realtime_clock_change(self: Arc<Self>, _seq: u64) {
        let mut state = self.state.lock_save_irq();
        self.sync_state_locked(&mut state);
        if state.is_read_ready() {
            state.waiters.wake_all();
        }
        drop(state);
        self.rearm_scheduled_timer();
    }

    fn release(self: &Arc<Self>) {
        if let Some(timer) = SYS_TIMER.get() {
            timer.remove_scheduled_timer(self.owner_tid, self.timer_id());
        }

        let mut state = self.state.lock_save_irq();
        self.unregister_realtime_listener_locked(&mut state);
        state.armed = None;
        state.expirations = 0;
        state.canceled = false;
    }
}

struct TimerFdWait {
    inner: Arc<TimerFdInner>,
    token: Option<u64>,
}

impl TimerFdWait {
    fn new(inner: Arc<TimerFdInner>) -> Self {
        Self { inner, token: None }
    }
}

impl Drop for TimerFdWait {
    fn drop(&mut self) {
        if let Some(token) = self.token.take() {
            self.inner.state.lock_save_irq().waiters.remove(token);
        }
    }
}

impl Future for TimerFdWait {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = unsafe { self.as_mut().get_unchecked_mut() };
        let mut state = this.inner.state.lock_save_irq();
        this.inner.sync_state_locked(&mut state);

        if state.is_read_ready() {
            if let Some(token) = this.token.take() {
                state.waiters.remove(token);
            }
            return Poll::Ready(());
        }

        if let Some(token) = this.token.take() {
            state.waiters.remove(token);
        }
        this.token = Some(state.waiters.register(cx.waker()));
        Poll::Pending
    }
}

#[async_trait::async_trait]
impl FileOps for TimerFd {
    async fn read(&mut self, ctx: &mut FileCtx, buf: UA, count: usize) -> Result<usize> {
        self.read_impl(buf, count, ctx.flags.contains(OpenFlags::O_NONBLOCK))
            .await
    }

    async fn readat(&mut self, buf: UA, count: usize, _offset: u64) -> Result<usize> {
        self.read_impl(buf, count, false).await
    }

    async fn writeat(&mut self, _buf: UA, _count: usize, _offset: u64) -> Result<usize> {
        Err(KernelError::InvalidValue)
    }

    fn poll_read_ready(&self) -> Pin<Box<dyn Future<Output = Result<()>> + 'static + Send>> {
        let inner = self.inner.clone();
        Box::pin(async move {
            TimerFdWait::new(inner).await;
            Ok(())
        })
    }

    async fn release(&mut self, _ctx: &FileCtx) -> Result<()> {
        self.inner.release();
        Ok(())
    }

    fn as_timerfd(&mut self) -> Option<&mut TimerFd> {
        Some(self)
    }
}

pub async fn sys_timerfd_create(ctx: &ProcessCtx, clockid: i32, flags: i32) -> Result<usize> {
    if flags & !TFD_CREATE_FLAGS != 0 {
        return Err(KernelError::InvalidValue);
    }

    let clock_id = match ClockId::try_from(clockid).map_err(|_| KernelError::InvalidValue)? {
        ClockId::Realtime => ClockId::Realtime,
        ClockId::Monotonic => ClockId::Monotonic,
        _ => return Err(KernelError::InvalidValue),
    };

    let file_flags = if flags & OpenFlags::O_NONBLOCK.bits() as i32 != 0 {
        OpenFlags::O_NONBLOCK
    } else {
        OpenFlags::empty()
    };
    let fd_flags = if flags & OpenFlags::O_CLOEXEC.bits() as i32 != 0 {
        FdFlags::CLOEXEC
    } else {
        FdFlags::empty()
    };

    let file = Arc::new(OpenFile::new(
        Box::new(TimerFd::new(clock_id, ctx.shared().tid())),
        file_flags,
    ));
    let fd = ctx
        .shared()
        .fd_table
        .lock_save_irq()
        .insert_with_flags(file, fd_flags)?;

    Ok(fd.as_raw() as usize)
}

pub async fn sys_timerfd_settime(
    ctx: &ProcessCtx,
    fd: Fd,
    flags: i32,
    new_value: TUA<ITimerSpec>,
    old_value: TUA<ITimerSpec>,
) -> Result<usize> {
    if flags & !TFD_SETTIME_FLAGS != 0 {
        return Err(KernelError::InvalidValue);
    }

    let new_value = copy_from_user(new_value).await?;
    new_value.validate()?;

    let file = ctx
        .shared()
        .fd_table
        .lock_save_irq()
        .get(fd)
        .ok_or(KernelError::BadFd)?;

    let outcome = {
        let (ops, _) = &mut *file.lock().await;
        let timerfd = ops.as_timerfd().ok_or(KernelError::InvalidValue)?;
        timerfd.inner().settime(flags, new_value)?
    };

    if !old_value.is_null() {
        copy_to_user(old_value, outcome.old_value).await?;
    }

    if outcome.canceled {
        Err(KernelError::Canceled)
    } else {
        Ok(0)
    }
}

pub async fn sys_timerfd_gettime(
    ctx: &ProcessCtx,
    fd: Fd,
    curr_value: TUA<ITimerSpec>,
) -> Result<usize> {
    let file = ctx
        .shared()
        .fd_table
        .lock_save_irq()
        .get(fd)
        .ok_or(KernelError::BadFd)?;

    let value = {
        let (ops, _) = &mut *file.lock().await;
        let timerfd = ops.as_timerfd().ok_or(KernelError::InvalidValue)?;
        let mut state = timerfd.inner().state.lock_save_irq();
        timerfd.inner().curr_value_locked(&mut state)
    };

    copy_to_user(curr_value, value).await?;
    Ok(0)
}
