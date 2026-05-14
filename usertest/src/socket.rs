use crate::register_test;
use libc::{AF_INET, AF_UNIX, SOCK_DGRAM, SOCK_STREAM};
use libc::{accept, bind, connect, listen, shutdown, socket};
use std::io::{Read, Write};
use std::net::{Ipv4Addr, TcpListener};
use std::ptr;

pub fn test_inet_socket_creation() {
    unsafe {
        let sockfd = socket(AF_INET, SOCK_STREAM, 0);
        if sockfd < 0 {
            panic!("Failed to create TCP socket");
        }

        let sockfd = socket(AF_INET, SOCK_DGRAM, 0);
        if sockfd < 0 {
            panic!("Failed to create UDP socket");
        }
    }
}

register_test!(test_inet_socket_creation);

const SERVER_IP: &str = "127.0.0.1";
const SERVER_PORT: u16 = 10000;

fn loopback_addr(port: u16) -> libc::sockaddr_in {
    let mut addr: libc::sockaddr_in = unsafe { std::mem::zeroed() };
    addr.sin_family = libc::AF_INET as libc::sa_family_t;
    addr.sin_port = port.to_be();
    addr.sin_addr.s_addr = u32::from_ne_bytes([127, 0, 0, 1]);
    addr
}

fn write_all_fd(fd: libc::c_int, buf: &[u8]) {
    let mut written = 0;
    while written < buf.len() {
        let n = unsafe {
            libc::write(
                fd,
                buf[written..].as_ptr() as *const libc::c_void,
                buf.len() - written,
            )
        };
        if n < 0 {
            panic!("write failed: {}", std::io::Error::last_os_error());
        }
        if n == 0 {
            panic!("write returned 0 before the full buffer was written");
        }
        written += n as usize;
    }
}

fn read_exact_fd(fd: libc::c_int, buf: &mut [u8]) {
    let mut read = 0;
    while read < buf.len() {
        let n = unsafe {
            libc::read(
                fd,
                buf[read..].as_mut_ptr() as *mut libc::c_void,
                buf.len() - read,
            )
        };
        if n < 0 {
            panic!("read failed: {}", std::io::Error::last_os_error());
        }
        if n == 0 {
            panic!("unexpected EOF while reading from socket");
        }
        read += n as usize;
    }
}

pub fn test_tcp_socket_bind() {
    unsafe {
        let sock_fd = libc::socket(libc::AF_INET, libc::SOCK_STREAM, 0);
        if sock_fd < 0 {
            panic!(
                "Socket creation failed. errno: {}",
                *libc::__errno_location()
            );
        }

        let mut server_addr: libc::sockaddr_in = std::mem::zeroed();
        server_addr.sin_family = libc::AF_INET as libc::sa_family_t;
        server_addr.sin_port = SERVER_PORT.to_be(); // Host to network byte order

        // Parse natively using Rust's compiler, handling any validation errors
        let ip: Ipv4Addr = SERVER_IP.parse().expect("Invalid IP address string");
        let ip_bytes = ip.octets(); // Gets [u8; 4]

        // Copy the byte-array directly into the network address field
        server_addr.sin_addr.s_addr = u32::from_ne_bytes(ip_bytes);

        let bind_res = libc::bind(
            sock_fd,
            &server_addr as *const libc::sockaddr_in as *const libc::sockaddr,
            std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t,
        );

        if bind_res < 0 {
            libc::close(sock_fd);
            panic!("Bind failed. errno: {}", *libc::__errno_location());
        }

        libc::close(sock_fd);
    }
}

register_test!(test_tcp_socket_bind);

pub fn test_tcp_socket_bind_rust() {
    let socket = TcpListener::bind((SERVER_IP, SERVER_PORT)).expect("Failed to bind TCP socket");
    drop(socket);
}

register_test!(test_tcp_socket_bind_rust);

pub fn test_tcp_client_server() {
    let server_port = 20_000 + (unsafe { libc::getpid() } % 20_000) as u16;
    let server_addr = loopback_addr(server_port);

    let server_fd = unsafe { socket(AF_INET, SOCK_STREAM, 0) };
    assert!(
        server_fd >= 0,
        "server socket creation failed: {}",
        std::io::Error::last_os_error()
    );

    let ret = unsafe {
        bind(
            server_fd,
            &server_addr as *const libc::sockaddr_in as *const libc::sockaddr,
            std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t,
        )
    };
    assert_eq!(ret, 0, "bind failed: {}", std::io::Error::last_os_error());

    let ret = unsafe { listen(server_fd, 1) };
    assert_eq!(ret, 0, "listen failed: {}", std::io::Error::last_os_error());

    let pid = unsafe { libc::fork() };
    if pid < 0 {
        panic!("fork failed: {}", std::io::Error::last_os_error());
    }

    if pid == 0 {
        unsafe {
            libc::close(server_fd);
        }

        let client_fd = unsafe { socket(AF_INET, SOCK_STREAM, 0) };
        assert!(
            client_fd >= 0,
            "client socket creation failed: {}",
            std::io::Error::last_os_error()
        );

        let ret = unsafe {
            connect(
                client_fd,
                &server_addr as *const libc::sockaddr_in as *const libc::sockaddr,
                std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t,
            )
        };
        assert_eq!(
            ret,
            0,
            "client connect failed: {}",
            std::io::Error::last_os_error()
        );

        write_all_fd(client_fd, b"hello");

        let mut buf = [0u8; 5];
        read_exact_fd(client_fd, &mut buf);
        assert_eq!(&buf, b"world");

        unsafe {
            libc::close(client_fd);
            libc::_exit(0);
        }
    } else {
        let conn_fd = unsafe { accept(server_fd, ptr::null_mut(), ptr::null_mut()) };
        assert!(
            conn_fd >= 0,
            "accept failed: {}",
            std::io::Error::last_os_error()
        );

        let mut buf = [0u8; 5];
        read_exact_fd(conn_fd, &mut buf);
        assert_eq!(&buf, b"hello");

        write_all_fd(conn_fd, b"world");

        unsafe {
            libc::close(conn_fd);
            libc::close(server_fd);
        }

        let mut status = 0;
        let waited = unsafe { libc::waitpid(pid, &mut status, 0) };
        assert_eq!(
            waited,
            pid,
            "waitpid failed: {}",
            std::io::Error::last_os_error()
        );
        assert!(
            libc::WIFEXITED(status),
            "client process did not exit normally"
        );
        assert_eq!(libc::WEXITSTATUS(status), 0, "client process failed");
    }
}

register_test!(test_tcp_client_server);

pub fn test_udp_poll_read_ready() {
    let recv_port = 30_000 + (unsafe { libc::getpid() } % 10_000) as u16;
    let recv_addr = loopback_addr(recv_port);

    unsafe {
        let recv_fd = socket(AF_INET, SOCK_DGRAM, 0);
        assert!(
            recv_fd >= 0,
            "recv socket creation failed: {}",
            std::io::Error::last_os_error()
        );

        let ret = bind(
            recv_fd,
            &recv_addr as *const libc::sockaddr_in as *const libc::sockaddr,
            std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t,
        );
        assert_eq!(
            ret,
            0,
            "udp bind failed: {}",
            std::io::Error::last_os_error()
        );

        let pid = libc::fork();
        if pid < 0 {
            panic!("fork failed: {}", std::io::Error::last_os_error());
        }

        if pid == 0 {
            libc::usleep(100_000);

            let send_fd = socket(AF_INET, SOCK_DGRAM, 0);
            if send_fd < 0 {
                panic!(
                    "send socket creation failed: {}",
                    std::io::Error::last_os_error()
                );
            }

            let msg = b"ping";
            let sent = libc::sendto(
                send_fd,
                msg.as_ptr().cast(),
                msg.len(),
                0,
                &recv_addr as *const libc::sockaddr_in as *const libc::sockaddr,
                std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t,
            );
            if sent != msg.len() as isize {
                panic!("sendto failed: {}", std::io::Error::last_os_error());
            }

            libc::close(send_fd);
            libc::_exit(0);
        }

        let mut pfd = libc::pollfd {
            fd: recv_fd,
            events: libc::POLLIN,
            revents: 0,
        };
        let ready = libc::poll(&mut pfd, 1, 1000);
        assert_eq!(ready, 1, "poll failed: {}", std::io::Error::last_os_error());
        assert_ne!(pfd.revents & libc::POLLIN, 0);

        let mut buf = [0u8; 4];
        let recvd = libc::recvfrom(
            recv_fd,
            buf.as_mut_ptr().cast(),
            buf.len(),
            0,
            ptr::null_mut(),
            ptr::null_mut(),
        );
        assert_eq!(recvd, buf.len() as isize);
        assert_eq!(&buf, b"ping");

        libc::close(recv_fd);

        let mut status = 0;
        let waited = libc::waitpid(pid, &mut status, 0);
        assert_eq!(
            waited,
            pid,
            "waitpid failed: {}",
            std::io::Error::last_os_error()
        );
        assert!(libc::WIFEXITED(status));
        assert_eq!(libc::WEXITSTATUS(status), 0);
    }
}

register_test!(test_udp_poll_read_ready);

fn getsockopt_int(fd: libc::c_int, level: libc::c_int, optname: libc::c_int) -> libc::c_int {
    let mut value: libc::c_int = -1;
    let mut len = std::mem::size_of_val(&value) as libc::socklen_t;
    let ret = unsafe {
        libc::getsockopt(
            fd,
            level,
            optname,
            &mut value as *mut _ as *mut libc::c_void,
            &mut len,
        )
    };
    assert_eq!(
        ret,
        0,
        "getsockopt failed: {}",
        std::io::Error::last_os_error()
    );
    assert_eq!(len as usize, std::mem::size_of_val(&value));
    value
}

pub fn test_socket_options() {
    let fd = unsafe { socket(AF_INET, SOCK_STREAM, 0) };
    assert!(
        fd >= 0,
        "socket failed: {}",
        std::io::Error::last_os_error()
    );

    let one: libc::c_int = 1;
    let ret = unsafe {
        libc::setsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_REUSEADDR,
            &one as *const _ as *const libc::c_void,
            std::mem::size_of_val(&one) as libc::socklen_t,
        )
    };
    assert_eq!(ret, 0, "setsockopt SO_REUSEADDR failed");
    assert_eq!(getsockopt_int(fd, libc::SOL_SOCKET, libc::SO_REUSEADDR), 1);
    assert_eq!(
        getsockopt_int(fd, libc::SOL_SOCKET, libc::SO_TYPE),
        SOCK_STREAM
    );

    let ret = unsafe {
        libc::setsockopt(
            fd,
            libc::IPPROTO_TCP,
            libc::TCP_NODELAY,
            &one as *const _ as *const libc::c_void,
            std::mem::size_of_val(&one) as libc::socklen_t,
        )
    };
    assert_eq!(ret, 0, "setsockopt TCP_NODELAY failed");
    assert_eq!(getsockopt_int(fd, libc::IPPROTO_TCP, libc::TCP_NODELAY), 1);

    let server_addr = loopback_addr(0);
    let ret = unsafe {
        bind(
            fd,
            &server_addr as *const libc::sockaddr_in as *const libc::sockaddr,
            std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t,
        )
    };
    assert_eq!(ret, 0, "bind failed: {}", std::io::Error::last_os_error());

    let ret = unsafe { listen(fd, 1) };
    assert_eq!(ret, 0, "listen failed: {}", std::io::Error::last_os_error());
    assert_eq!(getsockopt_int(fd, libc::SOL_SOCKET, libc::SO_ACCEPTCONN), 1);

    unsafe {
        libc::close(fd);
    }
}

register_test!(test_socket_options);

pub fn test_unix_socket_creation() {
    unsafe {
        let sockfd = socket(AF_UNIX, SOCK_STREAM, 0);
        if sockfd < 0 {
            panic!("Failed to create UNIX stream socket");
        }
    }
    unsafe {
        let sockfd = socket(AF_UNIX, SOCK_DGRAM, 0);
        if sockfd < 0 {
            panic!("Failed to create UNIX datagram socket");
        }
    }
}

register_test!(test_unix_socket_creation);

pub fn test_unix_socket_basic_functions() {
    let sockfd = unsafe { socket(AF_UNIX, SOCK_STREAM, 0) };
    if sockfd < 0 {
        panic!("Failed to create UNIX stream socket for function tests");
    }
    let path = "/tmp/test_socket";
    let sockaddr = libc::sockaddr_un {
        sun_family: AF_UNIX as u16,
        sun_path: {
            let mut path_array = [0u8; 108];
            for (i, &b) in path.as_bytes().iter().enumerate() {
                path_array[i] = b;
            }
            path_array
        },
    };
    let bind_result = unsafe {
        bind(
            sockfd,
            &sockaddr as *const libc::sockaddr_un as *const libc::sockaddr,
            std::mem::size_of::<libc::sockaddr_un>() as u32,
        )
    };
    if bind_result < 0 {
        panic!("Failed to bind UNIX socket");
    }
    let listen_result = unsafe { listen(sockfd, 5) };
    if listen_result < 0 {
        panic!("Failed to listen on UNIX socket");
    }
    let shutdown_result = unsafe { shutdown(sockfd, 2) };
    if shutdown_result < 0 {
        panic!("Failed to shutdown UNIX socket");
    }
}

register_test!(test_unix_socket_basic_functions);

pub fn test_unix_socket_fork_msg_passing() {
    use std::ptr;

    // Create server socket, bind and listen before fork
    let server_fd = unsafe { socket(AF_UNIX, SOCK_STREAM, 0) };
    if server_fd < 0 {
        panic!("Failed to create server UNIX socket");
    }

    let path = "/tmp/uds_fork_test";
    let sockaddr = libc::sockaddr_un {
        sun_family: AF_UNIX as u16,
        sun_path: {
            let mut path_array = [0u8; 108];
            for (i, &b) in path.as_bytes().iter().enumerate() {
                path_array[i] = b;
            }
            path_array
        },
    };

    let ret = unsafe {
        bind(
            server_fd,
            &sockaddr as *const libc::sockaddr_un as *const libc::sockaddr,
            std::mem::size_of::<libc::sockaddr_un>() as u32,
        )
    };
    if ret < 0 {
        panic!("Server bind failed");
    }
    let ret = unsafe { listen(server_fd, 1) };
    if ret < 0 {
        panic!("Server listen failed");
    }

    let pid = unsafe { libc::fork() };
    if pid < 0 {
        panic!("fork failed");
    }

    if pid == 0 {
        // Child: client
        let client_fd = unsafe { socket(AF_UNIX, SOCK_STREAM, 0) };
        if client_fd < 0 {
            panic!("Client socket creation failed");
        }
        let ret = unsafe {
            connect(
                client_fd,
                &sockaddr as *const libc::sockaddr_un as *const libc::sockaddr,
                std::mem::size_of::<libc::sockaddr_un>() as u32,
            )
        };
        if ret < 0 {
            panic!("Client connect failed");
        }

        // Send request
        let req = b"hello";
        let wr = unsafe { libc::write(client_fd, req.as_ptr() as *const _, req.len()) };
        if wr != req.len() as isize {
            panic!("Client write failed");
        }

        // Receive response
        let mut resp = [0u8; 5];
        let rd = unsafe { libc::read(client_fd, resp.as_mut_ptr() as *mut _, resp.len()) };
        if rd != resp.len() as isize || &resp != b"world" {
            panic!("Client read failed");
        }

        unsafe { libc::close(client_fd) };
        unsafe { libc::_exit(0) };
    } else {
        // Parent: server
        let conn_fd = unsafe { accept(server_fd, ptr::null_mut(), ptr::null_mut()) };
        if conn_fd < 0 {
            panic!("Server accept failed");
        }

        // Receive request
        let mut buf = [0u8; 5];
        let rd = unsafe { libc::read(conn_fd, buf.as_mut_ptr() as *mut _, buf.len()) };
        if rd != buf.len() as isize || &buf != b"hello" {
            panic!("Server read failed");
        }

        // Send response
        let resp = b"world";
        let wr = unsafe { libc::write(conn_fd, resp.as_ptr() as *const _, resp.len()) };
        if wr != resp.len() as isize {
            panic!("Server write failed");
        }

        // Wait for child
        let mut status = 0;
        unsafe { libc::waitpid(pid, &mut status, 0) };
        if !libc::WIFEXITED(status) || libc::WEXITSTATUS(status) != 0 {
            panic!("Client process did not exit cleanly");
        }

        unsafe { libc::close(conn_fd) };
        unsafe { libc::close(server_fd) };
    }
}

register_test!(test_unix_socket_fork_msg_passing);

pub fn test_rust_unix_socket() {
    use std::os::unix::net::{UnixListener, UnixStream};
    use std::thread;

    let path = "/tmp/rust_uds_test";
    let listener = UnixListener::bind(path).expect("Failed to bind UNIX socket");

    thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("Failed to accept connection");
        let mut buf = [0u8; 5];
        stream
            .read_exact(&mut buf)
            .expect("Failed to read from stream");
        if &buf != b"hello" {
            panic!("Server read incorrect data");
        }
        //     stream
        //         .write_all(b"world")
        //         .expect("Failed to write to stream");
    });

    let mut stream = UnixStream::connect(path).expect("Failed to connect to UNIX socket");
    stream
        .write_all(b"hello")
        .expect("Failed to write to stream");
    // let mut buf = [0u8; 5];
    // stream
    //     .read_exact(&mut buf)
    //     .expect("Failed to read from stream");
    // if &buf != b"world" {
    //     panic!("Client read incorrect data");
    // }
}

register_test!(test_rust_unix_socket);
