use crate::register_test;
use std::ffi::CString;

const KERNEL_SIGSET_SIZE: usize = std::mem::size_of::<u64>();

fn readlink_fd(fd: libc::c_int) -> String {
    let path = CString::new(format!("/proc/self/fd/{fd}")).unwrap();
    let mut buf = [0u8; 256];
    let len = unsafe {
        libc::readlink(
            path.as_ptr(),
            buf.as_mut_ptr() as *mut libc::c_char,
            buf.len(),
        )
    };

    assert!(
        len >= 0,
        "readlink failed: {}",
        std::io::Error::last_os_error()
    );

    String::from_utf8(buf[..len as usize].to_vec()).unwrap()
}

fn sigset(signals: &[libc::c_int]) -> libc::sigset_t {
    unsafe {
        let mut mask: libc::sigset_t = std::mem::zeroed();
        assert_eq!(libc::sigemptyset(&mut mask), 0);
        for &signal in signals {
            assert_eq!(libc::sigaddset(&mut mask, signal), 0);
        }
        mask
    }
}

unsafe fn signalfd4(fd: libc::c_int, mask: &libc::sigset_t, flags: libc::c_int) -> libc::c_int {
    unsafe {
        libc::syscall(
            libc::SYS_signalfd4,
            fd,
            mask as *const libc::sigset_t,
            KERNEL_SIGSET_SIZE,
            flags,
        ) as libc::c_int
    }
}

fn test_proc_self_fd_pipe_readlink() {
    unsafe {
        let mut fds = [0; 2];
        assert_eq!(libc::pipe(fds.as_mut_ptr()), 0);

        let read_end = readlink_fd(fds[0]);
        let write_end = readlink_fd(fds[1]);

        assert!(read_end.starts_with("pipe:[") && read_end.ends_with(']'));
        assert_eq!(read_end, write_end);

        libc::close(fds[0]);
        libc::close(fds[1]);
    }
}

register_test!(test_proc_self_fd_pipe_readlink);

fn test_proc_self_fd_epoll_readlink() {
    unsafe {
        let epfd = libc::epoll_create1(0);
        assert!(
            epfd >= 0,
            "epoll_create1 failed: {}",
            std::io::Error::last_os_error()
        );

        assert_eq!(readlink_fd(epfd), "anon_inode:[eventpoll]");

        libc::close(epfd);
    }
}

register_test!(test_proc_self_fd_epoll_readlink);

fn test_proc_self_fd_signalfd_readlink() {
    unsafe {
        let mask = sigset(&[libc::SIGUSR1]);
        let fd = signalfd4(-1, &mask, 0);
        assert!(
            fd >= 0,
            "signalfd4 failed: {}",
            std::io::Error::last_os_error()
        );

        assert_eq!(readlink_fd(fd), "anon_inode:[signalfd]");

        libc::close(fd);
    }
}

register_test!(test_proc_self_fd_signalfd_readlink);

fn test_proc_self_fd_socket_readlink() {
    unsafe {
        let fd = libc::socket(libc::AF_INET, libc::SOCK_STREAM, 0);
        assert!(
            fd >= 0,
            "socket failed: {}",
            std::io::Error::last_os_error()
        );

        let dup_fd = libc::dup(fd);
        assert!(
            dup_fd >= 0,
            "dup failed: {}",
            std::io::Error::last_os_error()
        );

        let target = readlink_fd(fd);
        assert!(target.starts_with("socket:[") && target.ends_with(']'));
        assert_eq!(target, readlink_fd(dup_fd));

        libc::close(dup_fd);
        libc::close(fd);
    }
}

register_test!(test_proc_self_fd_socket_readlink);
