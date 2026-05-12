use crate::register_test;
use std::mem::MaybeUninit;

const PID_FS_MAGIC: libc::c_long = 0x5049_4446;

fn test_pidfd_fstatfs() {
    unsafe {
        let pidfd =
            libc::syscall(libc::SYS_pidfd_open as libc::c_long, libc::getpid(), 0) as libc::c_int;
        if pidfd < 0 {
            panic!("pidfd_open failed: {}", std::io::Error::last_os_error());
        }

        let mut statfs = MaybeUninit::<libc::statfs>::uninit();
        if libc::fstatfs(pidfd, statfs.as_mut_ptr()) != 0 {
            let err = std::io::Error::last_os_error();
            libc::close(pidfd);
            panic!("fstatfs on pidfd failed: {err}");
        }

        let statfs = statfs.assume_init();
        assert_eq!(statfs.f_type as libc::c_long, PID_FS_MAGIC);

        libc::close(pidfd);
    }
}

register_test!(test_pidfd_fstatfs);
