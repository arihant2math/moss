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

const CLONE_PIDFD: u64 = 0x0000_1000;
const CLONE_PARENT_SETTID: u64 = 0x0010_0000;
const SYS_CLONE3: libc::c_long = 435;

fn test_clone3_process() {
    unsafe {
        let mut pidfd = -1i32;
        let mut parent_tid = 0u32;
        let args = CloneArgs {
            flags: CLONE_PIDFD | CLONE_PARENT_SETTID,
            pidfd: (&mut pidfd as *mut i32).addr() as u64,
            parent_tid: (&mut parent_tid as *mut u32).addr() as u64,
            exit_signal: libc::SIGCHLD as u64,
            ..Default::default()
        };

        let pid = libc::syscall(
            SYS_CLONE3 as _,
            &args as *const CloneArgs,
            core::mem::size_of::<CloneArgs>(),
        ) as libc::pid_t;

        if pid == -1 {
            panic!("clone3 failed: {}", std::io::Error::last_os_error());
        }

        if pid == 0 {
            libc::_exit(42);
        }

        assert_eq!(parent_tid, pid as u32);
        assert!(pidfd >= 0, "expected clone3 to return a pidfd");

        let mut status = 0;
        let waited = libc::waitpid(pid, &mut status, 0);
        assert_eq!(
            waited,
            pid,
            "waitpid failed: {}",
            std::io::Error::last_os_error()
        );
        assert!(libc::WIFEXITED(status));
        assert_eq!(libc::WEXITSTATUS(status), 42);

        assert_eq!(libc::close(pidfd), 0);
    }
}

crate::register_test!(test_clone3_process);
