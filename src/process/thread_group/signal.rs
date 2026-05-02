use crate::{memory::uaccess::UserCopyable, process::Task, sched::current_work};
use alloc::{collections::VecDeque, sync::Arc};
use bitflags::bitflags;
use core::{
    alloc::Layout,
    fmt::Display,
    mem::transmute,
    ops::{Index, IndexMut},
    sync::atomic::{AtomicU64, Ordering},
    task::Poll,
};
use ksigaction::{KSignalAction, UserspaceSigAction};
use libkernel::memory::{address::UA, region::UserMemoryRegion};

pub mod kill;
pub mod ksigaction;
pub mod sigaction;
pub mod sigaltstack;
pub mod signalfd;
pub mod sigprocmask;
mod uaccess;

bitflags! {
    #[repr(C)]
    #[derive(Debug, Clone, Copy)]
    pub struct SigSet: u64 {
       const SIGHUP     = 1 << 0;
       const SIGINT     = 1 << 1;
       const SIGQUIT    = 1 << 2;
       const SIGILL     = 1 << 3;
       const SIGTRAP    = 1 << 4;
       const SIGABRT    = 1 << 5;
       const SIGBUS     = 1 << 6;
       const SIGFPE     = 1 << 7;
       const SIGKILL    = 1 << 8;
       const SIGUSR1    = 1 << 9;
       const SIGSEGV    = 1 << 10;
       const SIGUSR2    = 1 << 11;
       const SIGPIPE    = 1 << 12;
       const SIGALRM    = 1 << 13;
       const SIGTERM    = 1 << 14;
       const SIGSTKFLT  = 1 << 15;
       const SIGCHLD    = 1 << 16;
       const SIGCONT    = 1 << 17;
       const SIGSTOP    = 1 << 18;
       const SIGTSTP    = 1 << 19;
       const SIGTTIN    = 1 << 20;
       const SIGTTOU    = 1 << 21;
       const SIGURG     = 1 << 22;
       const SIGXCPU    = 1 << 23;
       const SIGXFSZ    = 1 << 24;
       const SIGVTALRM  = 1 << 25;
       const SIGPROF    = 1 << 26;
       const SIGWINCH   = 1 << 27;
       const SIGIO      = 1 << 28;
       const SIGPWR     = 1 << 29;
       const SIGUNUSED  = 1u64 << 30;
       const SIGRTMIN   = 1u64 << 31;
       const SIGRT2     = 1u64 << 32;
       const SIGRT3     = 1u64 << 33;
       const SIGRT4     = 1u64 << 34;
       const SIGRT5     = 1u64 << 35;
       const SIGRT6     = 1u64 << 36;
       const SIGRT7     = 1u64 << 37;
       const SIGRT8     = 1u64 << 38;
       const SIGRT9     = 1u64 << 39;
       const SIGRT10    = 1u64 << 40;
       const SIGRT11    = 1u64 << 41;
       const SIGRT12    = 1u64 << 42;
       const SIGRT13    = 1u64 << 43;
       const SIGRT14    = 1u64 << 44;
       const SIGRT15    = 1u64 << 45;
       const SIGRT16    = 1u64 << 46;
       const SIGRT17    = 1u64 << 47;
       const SIGRT18    = 1u64 << 48;
       const SIGRT19    = 1u64 << 49;
       const SIGRT20    = 1u64 << 50;
       const SIGRT21    = 1u64 << 51;
       const SIGRT22    = 1u64 << 52;
       const SIGRT23    = 1u64 << 53;
       const SIGRT24    = 1u64 << 54;
       const SIGRT25    = 1u64 << 55;
       const SIGRT26    = 1u64 << 56;
       const SIGRT27    = 1u64 << 57;
       const SIGRT28    = 1u64 << 58;
       const SIGRT29    = 1u64 << 59;
       const SIGRT30    = 1u64 << 60;
       const SIGRT31    = 1u64 << 61;
       const SIGRT32    = 1u64 << 62;
       const SIGRTMAX   = 1u64 << 63;
       const UNMASKABLE_SIGNALS = Self::SIGKILL.bits() | Self::SIGSTOP.bits();
    }
}

unsafe impl UserCopyable for SigSet {}

impl From<SigId> for SigSet {
    fn from(value: SigId) -> Self {
        Self::from_bits_retain(1u64 << value as u32)
    }
}

impl From<SigSet> for SigId {
    fn from(value: SigSet) -> Self {
        debug_assert_eq!(value.bits().count_ones(), 1);

        let id = value.bits().trailing_zeros();

        if id > 63 {
            panic!("Unexpected signal id {id}");
        }

        // SAFETY: We have performed bounds checking above to ensure the value
        // is within the enum range
        unsafe { transmute::<u32, SigId>(id) }
    }
}

impl SigSet {
    /// Set the signal with id `signal` to true in the set.
    pub fn set_signal(&mut self, signal: SigId) {
        *self = self.union(signal.into());
    }

    /// Remove a set signal from the set, setting it to false, while respecting
    /// `mask`. Returns the ID of the removed signal.
    pub fn take_signal(&mut self, mask: SigSet) -> Option<SigId> {
        let signal = self.peek_signal(mask)?;

        self.remove(signal.into());

        Some(signal)
    }

    /// Check whether a signal is set in this set while repseciting the signal
    /// mask, `mask`. Returns the ID of the set signal.
    pub fn peek_signal(&self, mask: SigSet) -> Option<SigId> {
        let pending = self.difference(mask).bits();
        if pending == 0 {
            return None;
        }

        let id = pending.trailing_zeros();
        // SAFETY: `id` is a set bit in a u64-backed signal set, so it is in
        // the valid 0..=63 range covered by `SigId`.
        Some(unsafe { transmute::<u32, SigId>(id) })
    }
}

/// An atomically-accessible signal set.
pub struct AtomicSigSet(AtomicU64);

impl AtomicSigSet {
    pub const fn new(set: SigSet) -> Self {
        Self(AtomicU64::new(set.bits()))
    }

    pub const fn empty() -> Self {
        Self(AtomicU64::new(0))
    }

    /// Atomically insert a signal into the set.
    pub fn insert(&self, signal: SigSet) {
        self.0.fetch_or(signal.bits(), Ordering::Relaxed);
    }

    /// Check for a pending signal while respecting the mask, without removing
    /// it.
    pub fn peek_signal(&self, mask: SigSet) -> Option<SigId> {
        SigSet::from_bits_retain(self.0.load(Ordering::Relaxed)).peek_signal(mask)
    }

    /// Atomically remove and return a pending signal while respecting the mask.
    pub fn take_signal(&self, mask: SigSet) -> Option<SigId> {
        loop {
            let cur = self.0.load(Ordering::Relaxed);
            let set = SigSet::from_bits_retain(cur);
            let sig = set.peek_signal(mask)?;
            let new = set.difference(sig.into()).bits();
            match self
                .0
                .compare_exchange(cur, new, Ordering::Relaxed, Ordering::Relaxed)
            {
                Ok(_) => return Some(sig),
                Err(_) => continue,
            }
        }
    }

    /// Load the current signal set as a plain `SigSet`.
    pub fn load(&self) -> SigSet {
        SigSet::from_bits_retain(self.0.load(Ordering::Relaxed))
    }

    /// Store a full signal set.
    pub fn store(&self, set: SigSet) {
        self.0.store(set.bits(), Ordering::Relaxed);
    }
}

#[repr(u32)]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[allow(clippy::upper_case_acronyms)]
pub enum SigId {
    SIGHUP = 0,
    SIGINT = 1,
    SIGQUIT = 2,
    SIGILL = 3,
    SIGTRAP = 4,
    SIGABRT = 5,
    SIGBUS = 6,
    SIGFPE = 7,
    SIGKILL = 8,
    SIGUSR1 = 9,
    SIGSEGV = 10,
    SIGUSR2 = 11,
    SIGPIPE = 12,
    SIGALRM = 13,
    SIGTERM = 14,
    SIGSTKFLT = 15,
    SIGCHLD = 16,
    SIGCONT = 17,
    SIGSTOP = 18,
    SIGTSTP = 19,
    SIGTTIN = 20,
    SIGTTOU = 21,
    SIGURG = 22,
    SIGXCPU = 23,
    SIGXFSZ = 24,
    SIGVTALRM = 25,
    SIGPROF = 26,
    SIGWINCH = 27,
    SIGIO = 28,
    SIGPWR = 29,
    SIGUNUSED = 30,
    SIGRTMIN = 31,
    SIGRT2 = 32,
    SIGRT3 = 33,
    SIGRT4 = 34,
    SIGRT5 = 35,
    SIGRT6 = 36,
    SIGRT7 = 37,
    SIGRT8 = 38,
    SIGRT9 = 39,
    SIGRT10 = 40,
    SIGRT11 = 41,
    SIGRT12 = 42,
    SIGRT13 = 43,
    SIGRT14 = 44,
    SIGRT15 = 45,
    SIGRT16 = 46,
    SIGRT17 = 47,
    SIGRT18 = 48,
    SIGRT19 = 49,
    SIGRT20 = 50,
    SIGRT21 = 51,
    SIGRT22 = 52,
    SIGRT23 = 53,
    SIGRT24 = 54,
    SIGRT25 = 55,
    SIGRT26 = 56,
    SIGRT27 = 57,
    SIGRT28 = 58,
    SIGRT29 = 59,
    SIGRT30 = 60,
    SIGRT31 = 61,
    SIGRT32 = 62,
    SIGRTMAX = 63,
}

impl SigId {
    pub fn user_id(self) -> u64 {
        self as u64 + 1
    }

    pub fn is_realtime(self) -> bool {
        self as u32 >= Self::SIGRTMIN as u32
    }

    pub fn is_stopping(self) -> bool {
        matches!(
            self,
            Self::SIGSTOP | Self::SIGTSTP | Self::SIGTTIN | Self::SIGTTOU
        )
    }
}

impl Display for SigId {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        if self.is_realtime() {
            return write!(f, "SIGRT{}", self.user_id() - SigId::SIGRTMIN.user_id() + 1);
        }

        let set: SigSet = (*self).into();
        let name = set.iter_names().next().unwrap().0;
        f.write_str(name)
    }
}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct KSigInfo {
    pub signo: i32,
    pub errno: i32,
    pub code: i32,
    _pad0: i32,
    pub pid: i32,
    pub uid: u32,
    pub sigval: u64,
    _rest: [u8; 96],
}

unsafe impl UserCopyable for KSigInfo {}

impl KSigInfo {
    pub fn for_signal(signal: SigId) -> Self {
        Self {
            signo: signal.user_id() as i32,
            errno: 0,
            code: 0,
            _pad0: 0,
            pid: 0,
            uid: 0,
            sigval: 0,
            _rest: [0; 96],
        }
    }

    pub fn signal(&self) -> Option<SigId> {
        let signo = self.signo as u32;
        if (1..=64).contains(&signo) {
            // SAFETY: The range check above guarantees a valid signal id.
            Some(unsafe { transmute::<u32, SigId>(signo - 1) })
        } else {
            None
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct PendingSignal {
    pub id: SigId,
    pub info: KSigInfo,
}

impl PendingSignal {
    pub fn new(id: SigId) -> Self {
        Self {
            id,
            info: KSigInfo::for_signal(id),
        }
    }

    pub fn with_info(id: SigId, mut info: KSigInfo) -> Self {
        info.signo = id.user_id() as i32;
        Self { id, info }
    }
}

pub struct PendingSignals {
    set: SigSet,
    queue: VecDeque<PendingSignal>,
}

impl PendingSignals {
    pub fn empty() -> Self {
        Self {
            set: SigSet::empty(),
            queue: VecDeque::new(),
        }
    }

    pub fn from_set(set: SigSet) -> Self {
        Self {
            set,
            queue: VecDeque::new(),
        }
    }

    pub fn set(&self) -> SigSet {
        self.set
    }

    pub fn set_signal(&mut self, signal: SigId) {
        self.push_signal(PendingSignal::new(signal));
    }

    pub fn push_signal(&mut self, signal: PendingSignal) {
        if !signal.id.is_realtime() && self.set.contains(signal.id.into()) {
            return;
        }

        self.set.insert(signal.id.into());
        self.queue.push_back(signal);
    }

    pub fn peek_signal(&self, mask: SigSet) -> Option<SigId> {
        self.set.peek_signal(mask)
    }

    pub fn take_signal(&mut self, mask: SigSet) -> Option<PendingSignal> {
        let id = self.peek_signal(mask)?;

        let queued = self
            .queue
            .iter()
            .position(|signal| signal.id == id)
            .and_then(|idx| self.queue.remove(idx))
            .unwrap_or_else(|| PendingSignal::new(id));

        if id.is_realtime() && self.queue.iter().any(|signal| signal.id == id) {
            self.set.insert(id.into());
        } else {
            self.set.remove(id.into());
        }

        Some(queued)
    }
}

#[derive(Clone, Copy, Debug)]
pub enum SigActionState {
    Ignore,
    Default,
    Action(UserspaceSigAction),
}

#[derive(Clone)]
pub struct SigActionSet([SigActionState; 64]);

impl Index<SigId> for SigActionSet {
    type Output = SigActionState;

    fn index(&self, index: SigId) -> &Self::Output {
        self.0.index(index as usize)
    }
}

impl IndexMut<SigId> for SigActionSet {
    fn index_mut(&mut self, index: SigId) -> &mut Self::Output {
        self.0.index_mut(index as usize)
    }
}

#[derive(Clone)]
pub struct AltSigStack {
    range: UserMemoryRegion,
    ptr: UA,
}

pub struct AltStackAlloc {
    pub old_ptr: UA,
    pub data_ptr: UA,
}

impl AltSigStack {
    pub fn alloc_alt_stack<T>(&mut self) -> Option<AltStackAlloc> {
        let layout = Layout::new::<T>();
        let old_ptr = self.ptr;
        let new_ptr = self.ptr.sub_bytes(layout.size()).align(layout.align());

        if !self.range.contains_address(new_ptr) {
            None
        } else {
            self.ptr = new_ptr;
            Some(AltStackAlloc {
                old_ptr,
                data_ptr: new_ptr,
            })
        }
    }

    pub fn restore_alt_stack(&mut self, old_ptr: UA) {
        self.ptr = old_ptr;
    }

    pub fn in_use(&self) -> bool {
        self.ptr != self.range.end_address()
    }
}

#[derive(Clone)]
pub struct SignalActionState {
    action: SigActionSet,
    pub alt_stack: Option<AltSigStack>,
}

impl SignalActionState {
    pub fn new_ignore() -> Self {
        Self {
            action: SigActionSet([SigActionState::Ignore; 64]),
            alt_stack: None,
        }
    }

    pub fn new_default() -> Self {
        Self {
            action: SigActionSet([SigActionState::Default; 64]),
            alt_stack: None,
        }
    }

    pub fn action_signal(&self, id: SigId) -> Option<KSignalAction> {
        match self.action[id] {
            SigActionState::Ignore => None, // look for another signal,
            SigActionState::Default => KSignalAction::default_action(id),
            SigActionState::Action(userspace_sig_action) => {
                Some(KSignalAction::Userspace(id, userspace_sig_action))
            }
        }
    }
}

pub trait Interruptable<T, F: Future<Output = T>> {
    /// Mark this operation as interruptable.
    ///
    /// When a signal is delivered to this process/task while it is `Sleeping`,
    /// it may be woken up if there are no running tasks to deliver the signal
    /// to. If a task is running an `interruptable()` future, then the
    /// underlying future's execution will be short-circuted by the delivery of
    /// a signal. If the kernel is running a non-`interruptable()` future, then
    /// the signal delivery is deferred until either an `interruptable()`
    /// operation is executed or the system call has finished.
    ///
    /// `.await`ing a `interruptable()`-wrapped future returns a
    /// [InterruptResult].
    fn interruptable(self) -> InterruptableFut<T, F>;
}

/// A wrapper for a long-running future, allowing it to be interrupted by a
/// signal.
pub struct InterruptableFut<T, F: Future<Output = T>> {
    sub_fut: F,
    task: Arc<Task>,
}

impl<T, F: Future<Output = T>> Interruptable<T, F> for F {
    fn interruptable(self) -> InterruptableFut<T, F> {
        // TODO: Set the task state to a new variant `Interruptable`. This
        // allows the `deliver_signal` code to wake up a task to deliver a
        // signal to where it will be actioned.
        InterruptableFut {
            sub_fut: self,
            task: Arc::clone(&*current_work()),
        }
    }
}

impl<T, F: Future<Output = T>> Future for InterruptableFut<T, F> {
    type Output = InterruptResult<T>;

    fn poll(
        self: core::pin::Pin<&mut Self>,
        cx: &mut core::task::Context<'_>,
    ) -> Poll<Self::Output> {
        // Try the underlying future first.
        let this = unsafe { self.get_unchecked_mut() };
        let res = unsafe {
            core::pin::Pin::new_unchecked(&mut this.sub_fut)
                .poll(cx)
                .map(|x| InterruptResult::Uninterrupted(x))
        };

        if res.is_ready() {
            return res;
        }

        // See if there's a pending signal which interrupts this future.
        if this.task.peek_signal().is_some() {
            Poll::Ready(InterruptResult::Interrupted)
        } else {
            Poll::Pending
        }
    }
}

/// The result of running an interruptable operation within the kernel.
pub enum InterruptResult<T> {
    /// The operation was interrupted due to the delivery of the specified
    /// signal. The system call would normally short-circuit and return -EINTR
    /// at this point.
    Interrupted,
    /// The underlying future completed without interruption.
    Uninterrupted(T),
}
