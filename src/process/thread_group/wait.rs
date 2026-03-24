use super::{
    Pgid, Tgid, ThreadGroup,
    pid::PidT,
    signal::{InterruptResult, Interruptable, SigId},
};
use crate::memory::uaccess::{UserCopyable, copy_to_user};
use crate::sched::syscall_ctx::ProcessCtx;
use crate::sync::CondVar;
use crate::{clock::timespec::TimeSpec, process::Tid};
use alloc::collections::btree_map::BTreeMap;
use bitflags::Flags;
use libkernel::sync::condvar::WakeupType;
use libkernel::{
    error::{KernelError, Result},
    memory::address::TUA,
};

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct RUsage {
    pub ru_utime: TimeSpec, // user time used
    pub ru_stime: TimeSpec, // system time used
    pub ru_maxrss: i64,     // maximum resident set size
    pub ru_ixrss: i64,      // integral shared memory size
    pub ru_idrss: i64,      // integral unshared data size
    pub ru_isrss: i64,      // integral unshared stack size
    pub ru_minflt: i64,     // page reclaims
    pub ru_majflt: i64,     // page faults
    pub ru_nswap: i64,      // swaps
    pub ru_inblock: i64,    // block input operations
    pub ru_oublock: i64,    // block output operations
    pub ru_msgsnd: i64,     // messages sent
    pub ru_msgrcv: i64,     // messages received
    pub ru_nsignals: i64,   // signals received
    pub ru_nvcsw: i64,      // voluntary context switches
    pub ru_nivcsw: i64,     // involuntary context switches
}

bitflags::bitflags! {
    #[derive(Clone, Copy, Debug)]
    pub struct WaitFlags: u32 {
       const WNOHANG    = 0x00000001;
       const WSTOPPED   = 0x00000002;
       const WEXITED    = 0x00000004;
       const WCONTINUED = 0x00000008;
       const WNOWAIT    = 0x10000000;
       const WNOTHREAD  = 0x20000000;
       const WALL       = 0x40000000;
       const WCLONE     = 0x80000000;
    }
}

// TODO: more fields needed for full compatibility
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct SigInfo {
    pub signo: i32,
    pub code: i32,
    pub errno: i32,
}

unsafe impl UserCopyable for SigInfo {}

// si_code values for SIGCHLD
const CLD_EXITED: i32 = 1;
const CLD_KILLED: i32 = 2;
const CLD_DUMPED: i32 = 3;
const CLD_STOPPED: i32 = 4;
const CLD_TRAPPED: i32 = 5;
const CLD_CONTINUED: i32 = 6;

#[derive(Clone, Copy, Debug)]
pub enum ChildState {
    NormalExit { code: u32 },
    SignalExit { signal: SigId, core: bool },
    Stop { signal: SigId },
    Continue,
}

#[derive(Clone, Copy, Debug)]
pub struct TraceTrap {
    signal: SigId,
    mask: i32,
}

impl TraceTrap {
    pub fn new(signal: SigId, mask: i32) -> Self {
        Self { signal, mask }
    }
}

#[derive(Clone, Copy, Debug)]
enum WaitEvent {
    Child(ChildState),
    Ptrace(TraceTrap),
}

impl ChildState {
    fn matches_wait_flags(&self, flags: WaitFlags) -> bool {
        match self {
            ChildState::NormalExit { .. } | ChildState::SignalExit { .. } => {
                flags.contains(WaitFlags::WEXITED)
            }
            ChildState::Stop { .. } => flags.contains(WaitFlags::WSTOPPED),
            ChildState::Continue => flags.contains(WaitFlags::WCONTINUED),
        }
    }
}

struct NotifierState {
    children: BTreeMap<Tgid, ChildState>,
    ptrace: BTreeMap<Tid, TraceTrap>,
}

impl NotifierState {
    fn new() -> Self {
        Self {
            children: BTreeMap::new(),
            ptrace: BTreeMap::new(),
        }
    }
}

pub struct Notifiers {
    inner: CondVar<NotifierState>,
}

impl Default for Notifiers {
    fn default() -> Self {
        Self::new()
    }
}

impl Notifiers {
    pub fn new() -> Self {
        Self {
            inner: CondVar::new(NotifierState::new()),
        }
    }

    pub fn child_update(&self, tgid: Tgid, new_state: ChildState) {
        self.inner.update(|state| {
            state.children.insert(tgid, new_state);

            // Since some wakers may be conditional upon state update changes,
            // notify everyone whenever a child updates it's state.
            WakeupType::All
        });
    }

    pub fn ptrace_notify(&self, tid: Tid, ptrace_trap: TraceTrap) {
        self.inner.update(|state| {
            state.ptrace.insert(tid, ptrace_trap);

            // Since some wakers may be conditional upon state update changes,
            // notify everyone whenever a child updates it's state.
            WakeupType::All
        });
    }
}

fn find_child_event(
    children: &mut BTreeMap<Tgid, ChildState>,
    pid: PidT,
    flags: WaitFlags,
    remove_entry: bool,
) -> Option<(PidT, WaitEvent)> {
    let key = if pid == -1 {
        children.iter().find_map(|(k, v)| {
            if v.matches_wait_flags(flags) {
                Some(*k)
            } else {
                None
            }
        })
    } else if pid < -1 {
        // Wait for any child whose process group ID matches abs(pid)
        let target_pgid = Pgid((-pid) as u32);
        children.iter().find_map(|(k, v)| {
            if !v.matches_wait_flags(flags) {
                return None;
            }
            if let Some(tg) = ThreadGroup::get(*k) {
                if *tg.pgid.lock_save_irq() == target_pgid {
                    Some(*k)
                } else {
                    None
                }
            } else {
                None
            }
        })
    } else {
        children
            .get_key_value(&Tgid::from_pid_t(pid))
            .and_then(|(k, v)| {
                if v.matches_wait_flags(flags) {
                    Some(*k)
                } else {
                    None
                }
            })
    }?;

    if remove_entry {
        children
            .remove_entry(&key)
            .map(|(k, v)| (k.value() as PidT, WaitEvent::Child(v)))
    } else {
        children
            .get(&key)
            .map(|v| (key.value() as PidT, WaitEvent::Child(*v)))
    }
}

fn find_ptrace_event(
    ptrace: &mut BTreeMap<Tid, TraceTrap>,
    pid: PidT,
    remove_entry: bool,
) -> Option<(PidT, WaitEvent)> {
    // Ptrace events are always eligible for collection regardless of wait
    // flags. The WSTOPPED/WUNTRACED filtering only governs non-traced
    // group-stop events in the children map.
    let key = if pid == -1 {
        ptrace.keys().next().copied()
    } else if pid < -1 {
        // TODO: pgid matching for ptrace events
        None
    } else {
        let tid = Tid::from_pid_t(pid);
        ptrace.contains_key(&tid).then_some(tid)
    }?;

    let event = if remove_entry {
        ptrace.remove(&key)?
    } else {
        *ptrace.get(&key)?
    };

    Some((key.value() as PidT, WaitEvent::Ptrace(event)))
}

fn find_event(
    state: &mut NotifierState,
    pid: PidT,
    flags: WaitFlags,
    remove_entry: bool,
) -> Option<(PidT, WaitEvent)> {
    // Ptrace events are always eligible and take priority.
    find_ptrace_event(&mut state.ptrace, pid, remove_entry)
        .or_else(|| find_child_event(&mut state.children, pid, flags, remove_entry))
}

pub async fn sys_wait4(
    ctx: &ProcessCtx,
    pid: PidT,
    stat_addr: TUA<i32>,
    flags: u32,
    rusage: TUA<RUsage>,
) -> Result<usize> {
    let mut flags = WaitFlags::from_bits_retain(flags);

    if flags.contains_unknown_bits() {
        return Err(KernelError::InvalidValue);
    }

    // Check for valid flags.
    if !flags
        .difference(
            WaitFlags::WNOHANG
                | WaitFlags::WSTOPPED
                | WaitFlags::WCONTINUED
                | WaitFlags::WNOTHREAD
                | WaitFlags::WCLONE
                | WaitFlags::WALL,
        )
        .is_empty()
    {
        return Err(KernelError::InvalidValue);
    }

    // wait4 implies WEXITED.
    flags.insert(WaitFlags::WEXITED);

    if !rusage.is_null() {
        // TODO: Funky waiting.
        return Err(KernelError::NotSupported);
    }

    let task = ctx.shared();

    let child_proc_count = task.process.children.lock_save_irq().iter().count();

    let (ret_pid, event) = if child_proc_count == 0 || flags.contains(WaitFlags::WNOHANG) {
        // Special case for no children. See if there are any pending child
        // notification events without sleeping. If there are no children and no
        // pending events, return ECHILD.
        let mut ret = None;
        task.process.child_notifiers.inner.update(|s| {
            ret = find_event(s, pid, flags, true);
            WakeupType::None
        });

        match ret {
            Some(ret) => ret,
            None if child_proc_count == 0 => return Err(KernelError::NoChildProcess),
            None => return Ok(0),
        }
    } else {
        match task
            .process
            .child_notifiers
            .inner
            .wait_until(|state| find_event(state, pid, flags, true))
            .interruptable()
            .await
        {
            InterruptResult::Interrupted => return Err(KernelError::Interrupted),
            InterruptResult::Uninterrupted(r) => r,
        }
    };

    if !stat_addr.is_null() {
        match event {
            WaitEvent::Child(ChildState::NormalExit { code }) => {
                copy_to_user(stat_addr, (code as i32 & 0xff) << 8).await?;
            }
            WaitEvent::Child(ChildState::SignalExit { signal, core }) => {
                copy_to_user(
                    stat_addr,
                    (signal.user_id() as i32) | if core { 0x80 } else { 0x0 },
                )
                .await?;
            }
            WaitEvent::Child(ChildState::Stop { signal }) => {
                copy_to_user(stat_addr, ((signal.user_id() as i32) << 8) | 0x7f).await?;
            }
            WaitEvent::Ptrace(TraceTrap { signal, mask }) => {
                copy_to_user(
                    stat_addr,
                    ((signal.user_id() as i32) << 8) | 0x7f | mask << 8,
                )
                .await?;
            }
            WaitEvent::Child(ChildState::Continue) => {
                copy_to_user(stat_addr, 0xffff).await?;
            }
        }
    }

    Ok(ret_pid as _)
}

// idtype for waitid
#[repr(i32)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[allow(non_camel_case_types)]
pub enum IdType {
    P_ALL = 0,
    P_PID = 1,
    P_PGID = 2,
}

pub async fn sys_waitid(
    ctx: &ProcessCtx,
    idtype: i32,
    id: PidT,
    infop: TUA<SigInfo>,
    options: u32,
    rusage: TUA<RUsage>,
) -> Result<usize> {
    let which = match idtype {
        0 => IdType::P_ALL,
        1 => IdType::P_PID,
        2 => IdType::P_PGID,
        _ => return Err(KernelError::InvalidValue),
    };

    let flags = WaitFlags::from_bits_retain(options);

    if flags.contains_unknown_bits() {
        return Err(KernelError::InvalidValue);
    }

    // Validate options subset allowed for waitid
    if !flags
        .difference(
            WaitFlags::WNOHANG
                | WaitFlags::WSTOPPED
                | WaitFlags::WCONTINUED
                | WaitFlags::WEXITED
                | WaitFlags::WNOWAIT,
        )
        .is_empty()
    {
        return Err(KernelError::InvalidValue);
    }

    if !rusage.is_null() {
        todo!();
    }

    // Map which/id to pid selection used by our wait helpers
    let sel_pid: PidT = match which {
        IdType::P_ALL => -1,
        IdType::P_PID => id,
        IdType::P_PGID => -id.abs(), // negative means select by PGID in helpers
    };

    let task = ctx.shared();

    let child_proc_count = task.process.children.lock_save_irq().iter().count();

    // Try immediate check if no children or WNOHANG
    let event = if child_proc_count == 0 || flags.contains(WaitFlags::WNOHANG) {
        let mut ret: Option<WaitEvent> = None;

        task.process.child_notifiers.inner.update(|s| {
            // Don't consume on WNOWAIT.
            ret = find_event(s, sel_pid, flags, !flags.contains(WaitFlags::WNOWAIT))
                .map(|(_, event)| event);
            WakeupType::None
        });

        match ret {
            Some(ret) => ret,
            None if child_proc_count == 0 => return Err(KernelError::NoChildProcess),
            None => return Ok(0),
        }
    } else {
        // Wait until a child matches; first find key, then remove conditionally
        task.process
            .child_notifiers
            .inner
            .wait_until(|s| {
                // Don't consume on WNOWAIT.
                find_event(s, sel_pid, flags, !flags.contains(WaitFlags::WNOWAIT))
            })
            .await
            .1
    };

    // Populate siginfo
    if !infop.is_null() {
        let mut siginfo = SigInfo {
            signo: SigId::SIGCHLD.user_id() as i32,
            code: 0,
            errno: 0,
        };
        match event {
            WaitEvent::Child(ChildState::NormalExit { code }) => {
                siginfo.code = CLD_EXITED;
                siginfo.errno = code as i32;
            }
            WaitEvent::Child(ChildState::SignalExit { signal, core }) => {
                siginfo.code = if core { CLD_DUMPED } else { CLD_KILLED };
                siginfo.errno = signal.user_id() as i32;
            }
            WaitEvent::Child(ChildState::Stop { signal }) => {
                siginfo.code = CLD_STOPPED;
                siginfo.errno = signal.user_id() as i32;
            }
            WaitEvent::Ptrace(TraceTrap { signal, .. }) => {
                siginfo.code = CLD_TRAPPED;
                siginfo.errno = signal.user_id() as i32;
            }
            WaitEvent::Child(ChildState::Continue) => {
                siginfo.code = CLD_CONTINUED;
            }
        }
        copy_to_user(infop, siginfo).await?;
    }

    // If WNOWAIT was specified, don't consume the state; our helpers already honored that
    // Return 0 on success
    Ok(0)
}
