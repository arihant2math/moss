use crate::register_test;

fn test_poll_ignores_negative_fd() {
    unsafe {
        let mut fds = [0; 2];
        assert_eq!(libc::pipe(fds.as_mut_ptr()), 0, "pipe failed");

        let byte = [1u8; 1];
        assert_eq!(
            libc::write(fds[1], byte.as_ptr().cast(), byte.len()),
            byte.len() as isize,
            "write failed: {}",
            std::io::Error::last_os_error()
        );

        let mut poll_fds = [
            libc::pollfd {
                fd: -1,
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: fds[0],
                events: libc::POLLIN,
                revents: 0,
            },
        ];

        let ready = libc::poll(poll_fds.as_mut_ptr(), poll_fds.len() as _, 0);
        assert_eq!(ready, 1, "poll failed: {}", std::io::Error::last_os_error());
        assert_eq!(poll_fds[0].revents, 0);
        assert_ne!(poll_fds[1].revents & libc::POLLIN, 0);

        libc::close(fds[0]);
        libc::close(fds[1]);
    }
}

register_test!(test_poll_ignores_negative_fd);

fn test_poll_invalid_fd_sets_pollnval() {
    unsafe {
        let mut fds = [0; 2];
        assert_eq!(libc::pipe(fds.as_mut_ptr()), 0, "pipe failed");

        let invalid_fd = fds[0];
        libc::close(invalid_fd);
        libc::close(fds[1]);

        let mut poll_fds = [libc::pollfd {
            fd: invalid_fd,
            events: libc::POLLIN,
            revents: 0,
        }];

        let ready = libc::poll(poll_fds.as_mut_ptr(), poll_fds.len() as _, 0);
        assert_eq!(ready, 1, "poll failed: {}", std::io::Error::last_os_error());
        assert_ne!(poll_fds[0].revents & libc::POLLNVAL, 0);
    }
}

register_test!(test_poll_invalid_fd_sets_pollnval);
