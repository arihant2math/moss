use crate::memory::uaccess::{UserCopyable, copy_from_user, copy_to_user, copy_to_user_slice};
use crate::net::{AF_INET, AF_UNIX, IPPROTO_TCP, SOCK_STREAM, SocketLen};
use crate::sync::SpinLock;
use core::mem::size_of;
use libkernel::error::KernelError;
use libkernel::memory::address::{TUA, UA};

pub const SOL_IP: i32 = 0;
pub const SOL_SOCKET: i32 = 1;

pub const SO_REUSEADDR: i32 = 2;
pub const SO_TYPE: i32 = 3;
pub const SO_ERROR: i32 = 4;
pub const SO_BROADCAST: i32 = 6;
pub const SO_SNDBUF: i32 = 7;
pub const SO_RCVBUF: i32 = 8;
pub const SO_KEEPALIVE: i32 = 9;
pub const SO_LINGER: i32 = 13;
pub const SO_REUSEPORT: i32 = 15;
pub const SO_PASSCRED: i32 = 16;
pub const SO_RCVTIMEO: i32 = 20;
pub const SO_SNDTIMEO: i32 = 21;
pub const SO_ACCEPTCONN: i32 = 30;
pub const SO_PROTOCOL: i32 = 38;
pub const SO_DOMAIN: i32 = 39;

pub const IP_TTL: i32 = 2;
pub const IP_RECVERR: i32 = 11;

pub const TCP_NODELAY: i32 = 1;

const DEFAULT_BUFFER_SIZE: i32 = 64 * 1024;
const DEFAULT_IP_TTL: i32 = 64;

#[derive(Copy, Clone)]
pub struct SocketMeta {
    pub domain: i32,
    pub type_: i32,
    pub protocol: i32,
}

#[derive(Copy, Clone, Default)]
pub struct SocketRuntimeInfo {
    pub accept_conn: bool,
    pub error: i32,
}

#[repr(C)]
#[derive(Copy, Clone, Debug, Default)]
pub struct SocketTimeVal {
    pub tv_sec: i64,
    pub tv_usec: i64,
}

unsafe impl UserCopyable for SocketTimeVal {}

impl SocketTimeVal {
    fn validated(self) -> Result<Self, KernelError> {
        if self.tv_sec < 0 || !(0..1_000_000).contains(&self.tv_usec) {
            return Err(KernelError::InvalidValue);
        }

        Ok(self)
    }
}

#[repr(C)]
#[derive(Copy, Clone, Debug, Default)]
pub struct SocketLinger {
    pub l_onoff: i32,
    pub l_linger: i32,
}

unsafe impl UserCopyable for SocketLinger {}

impl SocketLinger {
    fn validated(self) -> Result<Self, KernelError> {
        if self.l_onoff < 0 || self.l_linger < 0 {
            return Err(KernelError::InvalidValue);
        }

        Ok(self)
    }
}

#[derive(Copy, Clone)]
pub struct SocketOptionState {
    reuse_addr: bool,
    reuse_port: bool,
    broadcast: bool,
    keepalive: bool,
    passcred: bool,
    send_buffer_size: i32,
    recv_buffer_size: i32,
    send_timeout: SocketTimeVal,
    recv_timeout: SocketTimeVal,
    linger: SocketLinger,
    tcp_nodelay: bool,
    ip_ttl: i32,
    // Linux-compatible compatibility flag for IPv4 extended error delivery.
    // We persist the option value even though MSG_ERRQUEUE delivery is not
    // implemented yet.
    ip_recverr: bool,
}

impl SocketOptionState {
    pub const fn new() -> Self {
        Self {
            reuse_addr: false,
            reuse_port: false,
            broadcast: false,
            keepalive: false,
            passcred: false,
            send_buffer_size: DEFAULT_BUFFER_SIZE,
            recv_buffer_size: DEFAULT_BUFFER_SIZE,
            send_timeout: SocketTimeVal {
                tv_sec: 0,
                tv_usec: 0,
            },
            recv_timeout: SocketTimeVal {
                tv_sec: 0,
                tv_usec: 0,
            },
            linger: SocketLinger {
                l_onoff: 0,
                l_linger: 0,
            },
            tcp_nodelay: false,
            ip_ttl: DEFAULT_IP_TTL,
            ip_recverr: false,
        }
    }
}

fn supports_tcp_options(meta: SocketMeta) -> bool {
    meta.domain == AF_INET && meta.type_ == SOCK_STREAM && meta.protocol == IPPROTO_TCP
}

fn supports_ip_options(meta: SocketMeta) -> bool {
    meta.domain == AF_INET
}

fn supports_unix_credentials(meta: SocketMeta) -> bool {
    meta.domain == AF_UNIX
}

async fn read_sockopt_value<T: UserCopyable>(
    optval: UA,
    optlen: SocketLen,
) -> Result<T, KernelError> {
    if optlen < size_of::<T>() {
        return Err(KernelError::InvalidValue);
    }

    copy_from_user(optval.cast()).await
}

async fn read_sockopt_int(optval: UA, optlen: SocketLen) -> Result<i32, KernelError> {
    read_sockopt_value(optval, optlen).await
}

fn bool_as_int(value: bool) -> i32 {
    value as i32
}

async fn write_sockopt_bytes(
    optval: UA,
    optlen: TUA<SocketLen>,
    bytes: &[u8],
) -> Result<(), KernelError> {
    if optlen.is_null() {
        return Err(KernelError::InvalidValue);
    }

    let user_len = copy_from_user(optlen).await?;
    let to_copy = bytes.len().min(user_len);
    if to_copy != 0 {
        copy_to_user_slice(&bytes[..to_copy], optval).await?;
    }
    copy_to_user(optlen, bytes.len()).await?;
    Ok(())
}

async fn write_sockopt_value<T: UserCopyable>(
    optval: UA,
    optlen: TUA<SocketLen>,
    value: &T,
) -> Result<(), KernelError> {
    let bytes =
        unsafe { core::slice::from_raw_parts((value as *const T).cast::<u8>(), size_of::<T>()) };
    write_sockopt_bytes(optval, optlen, bytes).await
}

pub async fn set_sockopt(
    state: &SpinLock<SocketOptionState>,
    meta: SocketMeta,
    level: i32,
    optname: i32,
    optval: UA,
    optlen: SocketLen,
) -> Result<(), KernelError> {
    match level {
        SOL_SOCKET => match optname {
            SO_REUSEADDR => {
                let value = read_sockopt_int(optval, optlen).await? != 0;
                state.lock_save_irq().reuse_addr = value;
                Ok(())
            }
            SO_REUSEPORT => {
                let value = read_sockopt_int(optval, optlen).await? != 0;
                state.lock_save_irq().reuse_port = value;
                Ok(())
            }
            SO_BROADCAST => {
                let value = read_sockopt_int(optval, optlen).await? != 0;
                state.lock_save_irq().broadcast = value;
                Ok(())
            }
            SO_KEEPALIVE => {
                let value = read_sockopt_int(optval, optlen).await? != 0;
                state.lock_save_irq().keepalive = value;
                Ok(())
            }
            SO_PASSCRED => {
                if !supports_unix_credentials(meta) {
                    return Err(KernelError::InvalidValue);
                }
                let value = read_sockopt_int(optval, optlen).await? != 0;
                state.lock_save_irq().passcred = value;
                Ok(())
            }
            SO_SNDBUF => {
                let value = read_sockopt_int(optval, optlen).await?;
                if value < 0 {
                    return Err(KernelError::InvalidValue);
                }
                state.lock_save_irq().send_buffer_size = value;
                Ok(())
            }
            SO_RCVBUF => {
                let value = read_sockopt_int(optval, optlen).await?;
                if value < 0 {
                    return Err(KernelError::InvalidValue);
                }
                state.lock_save_irq().recv_buffer_size = value;
                Ok(())
            }
            SO_LINGER => {
                let value = read_sockopt_value::<SocketLinger>(optval, optlen)
                    .await?
                    .validated()?;
                state.lock_save_irq().linger = value;
                Ok(())
            }
            SO_RCVTIMEO => {
                let value = read_sockopt_value::<SocketTimeVal>(optval, optlen)
                    .await?
                    .validated()?;
                state.lock_save_irq().recv_timeout = value;
                Ok(())
            }
            SO_SNDTIMEO => {
                let value = read_sockopt_value::<SocketTimeVal>(optval, optlen)
                    .await?
                    .validated()?;
                state.lock_save_irq().send_timeout = value;
                Ok(())
            }
            SO_TYPE | SO_ERROR | SO_ACCEPTCONN | SO_PROTOCOL | SO_DOMAIN => {
                Err(KernelError::InvalidValue)
            }
            _ => Err(KernelError::InvalidValue),
        },
        SOL_IP => {
            if !supports_ip_options(meta) {
                return Err(KernelError::InvalidValue);
            }

            match optname {
                IP_TTL => {
                    let value = read_sockopt_int(optval, optlen).await?;
                    let value = if value == -1 { DEFAULT_IP_TTL } else { value };
                    if !(1..=255).contains(&value) {
                        return Err(KernelError::InvalidValue);
                    }
                    state.lock_save_irq().ip_ttl = value;
                    Ok(())
                }
                IP_RECVERR => {
                    let value = read_sockopt_int(optval, optlen).await? != 0;
                    state.lock_save_irq().ip_recverr = value;
                    Ok(())
                }
                _ => Err(KernelError::InvalidValue),
            }
        }
        IPPROTO_TCP => {
            if !supports_tcp_options(meta) {
                return Err(KernelError::InvalidValue);
            }

            match optname {
                TCP_NODELAY => {
                    let value = read_sockopt_int(optval, optlen).await? != 0;
                    state.lock_save_irq().tcp_nodelay = value;
                    Ok(())
                }
                _ => Err(KernelError::InvalidValue),
            }
        }
        _ => Err(KernelError::InvalidValue),
    }
}

pub async fn get_sockopt(
    state: &SpinLock<SocketOptionState>,
    meta: SocketMeta,
    runtime: SocketRuntimeInfo,
    level: i32,
    optname: i32,
    optval: UA,
    optlen: TUA<SocketLen>,
) -> Result<(), KernelError> {
    let state = *state.lock_save_irq();

    match level {
        SOL_SOCKET => match optname {
            SO_REUSEADDR => {
                write_sockopt_value(optval, optlen, &bool_as_int(state.reuse_addr)).await
            }
            SO_REUSEPORT => {
                write_sockopt_value(optval, optlen, &bool_as_int(state.reuse_port)).await
            }
            SO_BROADCAST => {
                write_sockopt_value(optval, optlen, &bool_as_int(state.broadcast)).await
            }
            SO_KEEPALIVE => {
                write_sockopt_value(optval, optlen, &bool_as_int(state.keepalive)).await
            }
            SO_PASSCRED => {
                if !supports_unix_credentials(meta) {
                    return Err(KernelError::InvalidValue);
                }
                write_sockopt_value(optval, optlen, &bool_as_int(state.passcred)).await
            }
            SO_SNDBUF => write_sockopt_value(optval, optlen, &state.send_buffer_size).await,
            SO_RCVBUF => write_sockopt_value(optval, optlen, &state.recv_buffer_size).await,
            SO_TYPE => write_sockopt_value(optval, optlen, &meta.type_).await,
            SO_ERROR => write_sockopt_value(optval, optlen, &runtime.error).await,
            SO_LINGER => write_sockopt_value(optval, optlen, &state.linger).await,
            SO_RCVTIMEO => write_sockopt_value(optval, optlen, &state.recv_timeout).await,
            SO_SNDTIMEO => write_sockopt_value(optval, optlen, &state.send_timeout).await,
            SO_ACCEPTCONN => {
                write_sockopt_value(optval, optlen, &bool_as_int(runtime.accept_conn)).await
            }
            SO_PROTOCOL => write_sockopt_value(optval, optlen, &meta.protocol).await,
            SO_DOMAIN => write_sockopt_value(optval, optlen, &meta.domain).await,
            _ => Err(KernelError::InvalidValue),
        },
        SOL_IP => {
            if !supports_ip_options(meta) {
                return Err(KernelError::InvalidValue);
            }

            match optname {
                IP_TTL => write_sockopt_value(optval, optlen, &state.ip_ttl).await,
                IP_RECVERR => {
                    write_sockopt_value(optval, optlen, &bool_as_int(state.ip_recverr)).await
                }
                _ => Err(KernelError::InvalidValue),
            }
        }
        IPPROTO_TCP => {
            if !supports_tcp_options(meta) {
                return Err(KernelError::InvalidValue);
            }

            match optname {
                TCP_NODELAY => {
                    write_sockopt_value(optval, optlen, &bool_as_int(state.tcp_nodelay)).await
                }
                _ => Err(KernelError::InvalidValue),
            }
        }
        _ => Err(KernelError::InvalidValue),
    }
}
