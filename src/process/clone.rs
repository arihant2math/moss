use super::fd_table::{Fd, FileDescriptorTable};
use super::owned::OwnedTask;
use super::pidfd::{PidFile, PidfdFlags};
use super::ptrace::{PTrace, TracePoint, ptrace_stop};
use super::thread_group::ThreadGroup;
use super::{
    ITimers, Tid, VmHandle,
    ctx::Context,
    thread_group::signal::{AtomicSigSet, SigId, SigSet},
};
use crate::memory::uaccess::{copy_from_user_slice, copy_to_user};
use crate::sched::sched_task::Work;
use crate::sched::syscall_ctx::ProcessCtx;
use crate::{
    process::{TASK_LIST, Task},
    sched::{self},
    sync::SpinLock,
};
use alloc::{boxed::Box, sync::Arc};
use bitflags::bitflags;
use core::{mem::size_of, sync::atomic::AtomicUsize};
use libkernel::memory::address::TUA;
use libkernel::{
    error::{KernelError, Result},
    memory::address::UA,
    sync::waker_set::WakerSet,
};

pub static NUM_FORKS: AtomicUsize = AtomicUsize::new(0);

const CLONE3_ARGS_SIZE_VER0: usize = 64;

bitflags! {
    #[derive(Debug, Clone, Copy)]
    pub struct CloneFlags: u64 {
        const CLONE_VM = 0x0000_0100;
        const CLONE_FS = 0x0000_0200;
        const CLONE_FILES = 0x0000_0400;
        const CLONE_SIGHAND = 0x0000_0800;
        const CLONE_PIDFD = 0x0000_1000;
        const CLONE_PTRACE = 0x0000_2000;
        const CLONE_VFORK = 0x0000_4000;
        const CLONE_PARENT = 0x0000_8000;
        const CLONE_THREAD = 0x0001_0000;
        const CLONE_NEWNS = 0x0002_0000;
        const CLONE_SYSVSEM = 0x0004_0000;
        const CLONE_SETTLS = 0x0008_0000;
        const CLONE_PARENT_SETTID = 0x0010_0000;
        const CLONE_CHILD_CLEARTID = 0x0020_0000;
        const CLONE_DETACHED = 0x0040_0000;
        const CLONE_UNTRACED = 0x0080_0000;
        const CLONE_CHILD_SETTID = 0x0100_0000;
        const CLONE_NEWCGROUP = 0x0200_0000;
        const CLONE_NEWUTS = 0x0400_0000;
        const CLONE_NEWIPC = 0x0800_0000;
        const CLONE_NEWUSER = 0x1000_0000;
        const CLONE_NEWPID = 0x2000_0000;
        const CLONE_NEWNET = 0x4000_0000;
        const CLONE_IO = 0x8000_0000;
        const CLONE_CLEAR_SIGHAND = 0x0000_0001_0000_0000;
        const CLONE_INTO_CGROUP = 0x0000_0002_0000_0000;
    }
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
struct CloneArgs {
    flags: u64,
    pidfd: u64,
    child_tid: u64,
    parent_tid: u64,
    exit_signal: u64,
    stack: u64,
    stack_size: u64,
    tls: u64,
    set_tid: u64,
    set_tid_size: u64,
    cgroup: u64,
}

#[derive(Clone, Copy, Debug)]
struct CloneRequest {
    flags: CloneFlags,
    exit_signal: Option<SigId>,
    child_stack: Option<UA>,
    parent_tidptr: TUA<u32>,
    child_tidptr: TUA<u32>,
    pidfd_ptr: TUA<i32>,
    tls: usize,
    clone3: bool,
}

fn parse_exit_signal(raw: u64) -> Result<Option<SigId>> {
    if raw == 0 {
        return Ok(None);
    }

    if !(1..=31).contains(&raw) {
        return Err(KernelError::InvalidValue);
    }

    // SAFETY: We validated that the userspace-visible signal number is in the
    // range [1, 31]. `SigId` stores the zero-based signal number.
    Ok(Some(unsafe {
        core::mem::transmute::<u32, SigId>((raw as u32) - 1)
    }))
}

fn rollback_unpublished_child(
    parent: Option<&Arc<ThreadGroup>>,
    child_tgid: Option<super::thread_group::Tgid>,
    parent_fds: &Arc<SpinLock<FileDescriptorTable>>,
    pidfd_fd: Option<Fd>,
) {
    if let Some(fd) = pidfd_fd {
        parent_fds.lock_save_irq().remove(fd);
    }

    if let (Some(parent), Some(child_tgid)) = (parent, child_tgid) {
        parent.children.lock_save_irq().remove(&child_tgid);
    }
}

fn validate_clone_request(ctx: &ProcessCtx, req: CloneRequest) -> Result<()> {
    let flags = req.flags;

    if flags.contains(CloneFlags::CLONE_CLEAR_SIGHAND) && flags.contains(CloneFlags::CLONE_SIGHAND)
    {
        return Err(KernelError::InvalidValue);
    }

    if flags.contains(CloneFlags::CLONE_SIGHAND) && !flags.contains(CloneFlags::CLONE_VM) {
        return Err(KernelError::InvalidValue);
    }

    if flags.contains(CloneFlags::CLONE_THREAD)
        && (!flags.contains(CloneFlags::CLONE_SIGHAND) || !flags.contains(CloneFlags::CLONE_VM))
    {
        return Err(KernelError::InvalidValue);
    }

    if flags.contains(CloneFlags::CLONE_FS) && flags.contains(CloneFlags::CLONE_NEWNS) {
        return Err(KernelError::InvalidValue);
    }

    if flags.contains(CloneFlags::CLONE_NEWUSER) && flags.contains(CloneFlags::CLONE_FS) {
        return Err(KernelError::InvalidValue);
    }

    if flags.contains(CloneFlags::CLONE_NEWIPC) && flags.contains(CloneFlags::CLONE_SYSVSEM) {
        return Err(KernelError::InvalidValue);
    }

    if flags.contains(CloneFlags::CLONE_NEWPID)
        && flags.intersects(CloneFlags::CLONE_THREAD | CloneFlags::CLONE_PARENT)
    {
        return Err(KernelError::InvalidValue);
    }

    if flags.contains(CloneFlags::CLONE_NEWUSER) && flags.contains(CloneFlags::CLONE_THREAD) {
        return Err(KernelError::InvalidValue);
    }

    if flags.contains(CloneFlags::CLONE_PARENT) && ctx.task().process.tgid.is_init() {
        return Err(KernelError::InvalidValue);
    }

    if req.clone3 && flags.contains(CloneFlags::CLONE_DETACHED) {
        return Err(KernelError::InvalidValue);
    }

    if !req.clone3
        && flags.contains(CloneFlags::CLONE_PIDFD)
        && flags.contains(CloneFlags::CLONE_DETACHED)
    {
        return Err(KernelError::InvalidValue);
    }

    if flags.contains(CloneFlags::CLONE_PIDFD) && flags.contains(CloneFlags::CLONE_THREAD) {
        return Err(KernelError::InvalidValue);
    }

    if req.clone3
        && req.exit_signal.is_some()
        && flags.intersects(CloneFlags::CLONE_THREAD | CloneFlags::CLONE_PARENT)
    {
        return Err(KernelError::InvalidValue);
    }

    if flags.contains(CloneFlags::CLONE_PARENT_SETTID) && req.parent_tidptr.is_null() {
        return Err(KernelError::Fault);
    }

    if flags.contains(CloneFlags::CLONE_CHILD_SETTID) && req.child_tidptr.is_null() {
        return Err(KernelError::Fault);
    }

    if flags.contains(CloneFlags::CLONE_PIDFD) && req.pidfd_ptr.is_null() {
        return Err(KernelError::Fault);
    }

    if flags.contains(CloneFlags::CLONE_INTO_CGROUP) {
        return Err(KernelError::OpNotSupported);
    }

    if flags.intersects(
        CloneFlags::CLONE_NEWNS
            | CloneFlags::CLONE_NEWCGROUP
            | CloneFlags::CLONE_NEWUTS
            | CloneFlags::CLONE_NEWIPC
            | CloneFlags::CLONE_NEWUSER
            | CloneFlags::CLONE_NEWPID
            | CloneFlags::CLONE_NEWNET,
    ) {
        return Err(KernelError::InvalidValue);
    }

    if let Some(child_stack) = req.child_stack
        && (child_stack.value() & 0xf) != 0
    {
        return Err(KernelError::InvalidValue);
    }

    Ok(())
}

async fn copy_clone3_args(args: TUA<u8>, size: usize) -> Result<CloneArgs> {
    if size < CLONE3_ARGS_SIZE_VER0 {
        return Err(KernelError::InvalidValue);
    }

    let mut raw = [0u8; size_of::<CloneArgs>()];
    let copy_len = size.min(raw.len());

    copy_from_user_slice(args.to_untyped(), &mut raw[..copy_len]).await?;

    // SAFETY: `CloneArgs` is a plain old data structure consisting only of
    // `u64` fields and `raw` contains at least the first `copy_len` bytes with
    // the remainder zero-filled above.
    Ok(unsafe { raw.as_ptr().cast::<CloneArgs>().read_unaligned() })
}

fn clone3_child_stack(flags: CloneFlags, stack: UA, stack_size: usize) -> Result<Option<UA>> {
    if stack.is_null() && stack_size == 0 {
        if flags.contains(CloneFlags::CLONE_VM) {
            return Err(KernelError::InvalidValue);
        }

        return Ok(None);
    }

    if stack.is_null() || stack_size == 0 {
        return Err(KernelError::InvalidValue);
    }

    let top = stack
        .value()
        .checked_add(stack_size)
        .ok_or(KernelError::InvalidValue)?;

    Ok(Some(UA::from_value(top)))
}

async fn do_clone(ctx: &ProcessCtx, req: CloneRequest) -> Result<usize> {
    validate_clone_request(ctx, req)?;

    let trace_point = if req.flags.contains(CloneFlags::CLONE_THREAD) {
        TracePoint::Clone
    } else {
        TracePoint::Fork
    };

    // TODO: differentiate between `TracePoint::Fork`, `TracePoint::Clone` and
    // `TracePoint::VFork`.
    let should_trace_new_tsk = ptrace_stop(ctx, trace_point).await;

    let current_task = ctx.task();
    let parent_fds = current_task.fd_table.clone();

    let tid = Tid::next_tid();
    let mut child_parent = None;

    let mut user_ctx = *current_task.ctx.user();

    // TODO: Make this arch independent. The child returns '0' on clone.
    user_ctx.x[0] = 0;

    if let Some(child_stack) = req.child_stack {
        // TODO: Make this arch independent.
        user_ctx.sp_el0 = child_stack.value() as _;
    }

    if req.flags.contains(CloneFlags::CLONE_SETTLS) {
        // TODO: Make this arch independent.
        user_ctx.tpid_el0 = req.tls as _;
    }

    let tg = if req.flags.contains(CloneFlags::CLONE_THREAD) {
        current_task.process.clone()
    } else {
        let tgid_parent = if req.flags.contains(CloneFlags::CLONE_PARENT) {
            current_task
                .process
                .parent
                .lock_save_irq()
                .clone()
                .and_then(|p| p.upgrade())
                .ok_or(KernelError::InvalidValue)?
        } else {
            current_task.process.clone()
        };

        child_parent = Some(tgid_parent.clone());
        tgid_parent.new_child(
            req.flags.contains(CloneFlags::CLONE_SIGHAND),
            req.flags.contains(CloneFlags::CLONE_CLEAR_SIGHAND),
            req.flags.contains(CloneFlags::CLONE_VM)
                && !req.flags.contains(CloneFlags::CLONE_VFORK),
            tid,
            req.exit_signal,
        )
    };

    let vm = if req.flags.contains(CloneFlags::CLONE_VM) {
        if req.flags.contains(CloneFlags::CLONE_THREAD) {
            current_task.vm.clone()
        } else {
            Arc::new(VmHandle::from_shared(current_task.vm.shared_vm()))
        }
    } else {
        let proc_vm = current_task.vm.shared_vm();
        Arc::new(VmHandle::new(proc_vm.lock_save_irq().clone_as_cow()?))
    };

    let files = if req.flags.contains(CloneFlags::CLONE_FILES) {
        current_task.fd_table.clone()
    } else {
        Arc::new(SpinLock::new(current_task.fd_table.lock_save_irq().clone()))
    };

    let cwd = if req.flags.contains(CloneFlags::CLONE_FS) {
        current_task.cwd.clone()
    } else {
        Arc::new(SpinLock::new(current_task.cwd.lock_save_irq().clone()))
    };

    let root = if req.flags.contains(CloneFlags::CLONE_FS) {
        current_task.root.clone()
    } else {
        Arc::new(SpinLock::new(current_task.root.lock_save_irq().clone()))
    };

    let ptrace = if req.flags.contains(CloneFlags::CLONE_PTRACE) || should_trace_new_tsk {
        current_task.ptrace.lock_save_irq().clone()
    } else {
        PTrace::new()
    };

    let creds = current_task.creds.lock_save_irq().clone();
    let new_sigmask = AtomicSigSet::new(current_task.sig_mask.load());

    let initial_signals = if should_trace_new_tsk {
        // When we want to trace a new task through one of
        // PTRACE_O_TRACE{FORK,VFORK,CLONE}, stop the child as soon as
        // it is created.
        AtomicSigSet::new(SigSet::SIGSTOP)
    } else {
        AtomicSigSet::empty()
    };

    let new_task = OwnedTask {
        ctx: Context::from_user_ctx(user_ctx),
        priority: current_task.priority,
        robust_list: None,
        child_tid_ptr: req
            .flags
            .contains(CloneFlags::CLONE_CHILD_CLEARTID)
            .then_some(req.child_tidptr),
        t_shared: Arc::new(Task {
            tid,
            comm: Arc::new(SpinLock::new(*current_task.comm.lock_save_irq())),
            process: tg,
            vm,
            fd_table: files,
            cwd,
            root,
            i_timers: SpinLock::new(ITimers::default()),
            creds: SpinLock::new(creds),
            ptrace: SpinLock::new(ptrace),
            sig_mask: new_sigmask,
            pending_signals: initial_signals,
            signal_notifier: SpinLock::new(WakerSet::new()),
            utime: AtomicUsize::new(0),
            stime: AtomicUsize::new(0),
            last_account: AtomicUsize::new(0),
        }),
        in_syscall: false,
    };

    if req.flags.contains(CloneFlags::CLONE_VFORK) {
        new_task.process.start_vfork();
    }

    let desc = new_task.descriptor();
    let child_tgid = (!req.flags.contains(CloneFlags::CLONE_THREAD)).then_some(desc.tgid());
    let mut pidfd_fd = None;

    if req.flags.contains(CloneFlags::CLONE_PIDFD) {
        let file = PidFile::new_open_file(desc.tid(), PidfdFlags::empty());
        let fd = parent_fds.lock_save_irq().insert(file)?;

        if let Err(err) = copy_to_user(req.pidfd_ptr, fd.as_raw()).await {
            rollback_unpublished_child(child_parent.as_ref(), child_tgid, &parent_fds, Some(fd));
            return Err(err);
        }

        pidfd_fd = Some(fd);
    }

    if req.flags.contains(CloneFlags::CLONE_PARENT_SETTID)
        && let Err(err) = copy_to_user(req.parent_tidptr, desc.tid().value()).await
    {
        rollback_unpublished_child(child_parent.as_ref(), child_tgid, &parent_fds, pidfd_fd);
        return Err(err);
    }

    if req.flags.contains(CloneFlags::CLONE_CHILD_SETTID)
        && let Err(err) = copy_to_user(req.child_tidptr, desc.tid().value()).await
    {
        rollback_unpublished_child(child_parent.as_ref(), child_tgid, &parent_fds, pidfd_fd);
        return Err(err);
    }

    let work = Work::new(Box::new(new_task));
    let vfork_process = req
        .flags
        .contains(CloneFlags::CLONE_VFORK)
        .then(|| work.process.clone());

    TASK_LIST
        .lock_save_irq()
        .insert(desc.tid(), Arc::downgrade(&work));

    work.process
        .tasks
        .lock_save_irq()
        .insert(desc.tid(), Arc::downgrade(&work));

    sched::insert_work_cross_cpu(work);

    NUM_FORKS.fetch_add(1, core::sync::atomic::Ordering::Relaxed);

    if let Some(vfork_process) = vfork_process {
        vfork_process.wait_for_vfork_release().await;
    }

    Ok(desc.tid().value() as _)
}

pub async fn sys_clone(
    ctx: &ProcessCtx,
    flags: u32,
    newsp: UA,
    parent_tidptr: TUA<u32>,
    child_tidptr: TUA<u32>,
    tls: usize,
) -> Result<usize> {
    let flags_bits = flags as u64;
    let exit_signal = parse_exit_signal(flags_bits & 0xff)?;
    let flags = CloneFlags::from_bits(flags_bits & !0xff).ok_or(KernelError::InvalidValue)?;

    if flags.contains(CloneFlags::CLONE_PIDFD) && flags.contains(CloneFlags::CLONE_PARENT_SETTID) {
        return Err(KernelError::InvalidValue);
    }

    do_clone(
        ctx,
        CloneRequest {
            flags,
            exit_signal,
            child_stack: (!newsp.is_null()).then_some(newsp),
            parent_tidptr: if flags.contains(CloneFlags::CLONE_PIDFD) {
                TUA::null()
            } else {
                parent_tidptr
            },
            child_tidptr,
            pidfd_ptr: if flags.contains(CloneFlags::CLONE_PIDFD) {
                parent_tidptr.to_untyped().cast()
            } else {
                TUA::null()
            },
            tls,
            clone3: false,
        },
    )
    .await
}

pub async fn sys_clone3(ctx: &ProcessCtx, cl_args: TUA<u8>, size: usize) -> Result<usize> {
    let args = copy_clone3_args(cl_args, size).await?;
    let flags = CloneFlags::from_bits(args.flags).ok_or(KernelError::InvalidValue)?;

    if args.set_tid_size != 0 || args.set_tid != 0 {
        return Err(KernelError::InvalidValue);
    }

    let child_stack =
        clone3_child_stack(flags, UA::from_value(args.stack as _), args.stack_size as _)?;

    do_clone(
        ctx,
        CloneRequest {
            flags,
            exit_signal: parse_exit_signal(args.exit_signal)?,
            child_stack,
            parent_tidptr: TUA::from_value(args.parent_tid as _),
            child_tidptr: TUA::from_value(args.child_tid as _),
            pidfd_ptr: TUA::from_value(args.pidfd as _),
            tls: args.tls as usize,
            clone3: true,
        },
    )
    .await
}
