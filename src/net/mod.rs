mod sops;
pub mod syscalls;
pub(crate) mod tcp;
pub(crate) mod udp;
mod unix;

use crate::drivers::timer::now;
use crate::drivers::virtio_hal::VirtioHal;
use crate::memory::uaccess::{copy_from_user, copy_from_user_slice};
use crate::sync::{OnceLock, SpinLock};
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use core::net::Ipv4Addr;
use core::sync::atomic::{AtomicUsize, Ordering};
use core::time::Duration;
use libkernel::error::KernelError;
use libkernel::memory::address::UA;
use libkernel::sync::waker_set::WakerSet;
use smoltcp::iface::{Config as IfaceConfig, Interface, SocketSet};
use smoltcp::phy::{self, DeviceCapabilities, Medium};
use smoltcp::socket::tcp as smol_tcp;
use smoltcp::time::Instant as SmolInstant;
use smoltcp::wire::{
    EthernetAddress, HardwareAddress, IpAddress, IpCidr, IpEndpoint, IpListenEndpoint, Ipv4Address,
};
pub use sops::SocketOps;
use virtio_drivers::device::net::{RxBuffer, VirtIONet};
use virtio_drivers::transport::mmio::MmioTransport;

const VIRTIO_NET_QUEUE_SIZE: usize = 16;
const VIRTIO_NET_RX_BUFFER_LEN: usize = 2048;
const DEFAULT_IPV4_ADDR: [u8; 4] = [10, 0, 2, 15];
const DEFAULT_IPV4_GATEWAY: [u8; 4] = [10, 0, 2, 2];
const DEFAULT_WAIT: Duration = Duration::from_millis(10);

static SOCKET_WAIT_QUEUE: OnceLock<SpinLock<WakerSet>> = OnceLock::new();
static NET_DEVICE: OnceLock<Arc<SpinLock<VirtioNetHardware>>> = OnceLock::new();
static NET_CORE: OnceLock<SpinLock<NetCore>> = OnceLock::new();
static NEXT_EPHEMERAL_PORT: AtomicUsize = AtomicUsize::new(49152);

fn socket_wait_queue() -> &'static SpinLock<WakerSet> {
    SOCKET_WAIT_QUEUE.get_or_init(|| SpinLock::new(WakerSet::new()))
}

pub const AF_UNIX: i32 = 1;
pub const AF_INET: i32 = 2;
pub const SOCK_STREAM: i32 = 1;
pub const SOCK_DGRAM: i32 = 2;
pub const SOCK_SEQPACKET: i32 = 5;
pub const IPPROTO_TCP: i32 = 6;
pub const IPPROTO_UDP: i32 = 17;

// TODO: Needs to be u32
pub type SocketLen = usize;

type VirtioNetInner = VirtIONet<VirtioHal, MmioTransport<'static>, VIRTIO_NET_QUEUE_SIZE>;

struct VirtioNetHardware {
    net: VirtioNetInner,
}

#[derive(Clone)]
struct VirtioSmoltcpDevice {
    hw: Arc<SpinLock<VirtioNetHardware>>,
}

struct VirtioRxToken {
    hw: Arc<SpinLock<VirtioNetHardware>>,
    rx_buf: Option<RxBuffer>,
}

struct VirtioTxToken {
    hw: Arc<SpinLock<VirtioNetHardware>>,
}

struct NetCore {
    iface: Interface,
    device: VirtioSmoltcpDevice,
    sockets: SocketSet<'static>,
}

impl phy::Device for VirtioSmoltcpDevice {
    type RxToken<'a>
        = VirtioRxToken
    where
        Self: 'a;
    type TxToken<'a>
        = VirtioTxToken
    where
        Self: 'a;

    fn receive(
        &mut self,
        _timestamp: SmolInstant,
    ) -> Option<(Self::RxToken<'_>, Self::TxToken<'_>)> {
        let rx_buf = self.hw.lock_save_irq().net.receive().ok()?;
        Some((
            VirtioRxToken {
                hw: self.hw.clone(),
                rx_buf: Some(rx_buf),
            },
            VirtioTxToken {
                hw: self.hw.clone(),
            },
        ))
    }

    fn transmit(&mut self, _timestamp: SmolInstant) -> Option<Self::TxToken<'_>> {
        if self.hw.lock_save_irq().net.can_send() {
            Some(VirtioTxToken {
                hw: self.hw.clone(),
            })
        } else {
            None
        }
    }

    fn capabilities(&self) -> DeviceCapabilities {
        let mut caps = DeviceCapabilities::default();
        caps.medium = Medium::Ethernet;
        caps.max_transmission_unit = 1514;
        caps.max_burst_size = Some(1);
        caps
    }
}

impl phy::RxToken for VirtioRxToken {
    fn consume<R, F>(mut self, f: F) -> R
    where
        F: FnOnce(&[u8]) -> R,
    {
        let packet = self
            .rx_buf
            .as_ref()
            .map(RxBuffer::packet)
            .expect("virtio rx token missing buffer");
        let result = f(packet);
        if let Some(rx_buf) = self.rx_buf.take() {
            let _ = self.hw.lock_save_irq().net.recycle_rx_buffer(rx_buf);
        }
        result
    }
}

impl Drop for VirtioRxToken {
    fn drop(&mut self) {
        if let Some(rx_buf) = self.rx_buf.take() {
            let _ = self.hw.lock_save_irq().net.recycle_rx_buffer(rx_buf);
        }
    }
}

impl phy::TxToken for VirtioTxToken {
    fn consume<R, F>(self, len: usize, f: F) -> R
    where
        F: FnOnce(&mut [u8]) -> R,
    {
        let mut hw = self.hw.lock_save_irq();
        let mut tx_buf = hw.net.new_tx_buffer(len);
        let result = f(tx_buf.packet_mut());
        let _ = hw.net.send(tx_buf);
        result
    }
}

#[inline]
fn smol_now() -> SmolInstant {
    match now() {
        Some(instant) => {
            let dur: Duration = instant.into();
            let micros = dur.as_micros().min(i64::MAX as u128) as i64;
            SmolInstant::from_micros(micros)
        }
        None => SmolInstant::ZERO,
    }
}

fn net_core() -> Result<&'static SpinLock<NetCore>, KernelError> {
    NET_CORE.get().ok_or(KernelError::NotSupported)
}

fn with_net_core<R>(f: impl FnOnce(&mut NetCore) -> R) -> Result<R, KernelError> {
    let mut core = net_core()?.lock_save_irq();
    Ok(f(&mut core))
}

pub fn init_virtio_net(transport: MmioTransport<'static>) -> Result<(), KernelError> {
    let net =
        VirtIONet::<VirtioHal, _, VIRTIO_NET_QUEUE_SIZE>::new(transport, VIRTIO_NET_RX_BUFFER_LEN)
            .map_err(|_| KernelError::Other("virtio-net init failed"))?;

    let hw = Arc::new(SpinLock::new(VirtioNetHardware { net }));
    hw.lock_save_irq().net.enable_interrupts();

    if NET_DEVICE.set(hw.clone()).is_err() {
        return Err(KernelError::InUse);
    }

    let device = VirtioSmoltcpDevice { hw: hw.clone() };
    let mac = hw.lock_save_irq().net.mac_address();
    let mut init_device = device.clone();
    let mut iface = Interface::new(
        IfaceConfig::new(HardwareAddress::Ethernet(EthernetAddress(mac))),
        &mut init_device,
        smol_now(),
    );

    let ipv4_addr = Ipv4Address::from_octets(DEFAULT_IPV4_ADDR);
    let gateway = Ipv4Address::from_octets(DEFAULT_IPV4_GATEWAY);
    iface.update_ip_addrs(|ips| {
        ips.push(IpCidr::new(IpAddress::Ipv4(ipv4_addr), 24))
            .expect("virtio-net: ip address table full");
    });
    let _ = iface.routes_mut().add_default_ipv4_route(gateway);

    NET_CORE
        .set(SpinLock::new(NetCore {
            iface,
            device,
            sockets: SocketSet::new(vec![]),
        }))
        .map_err(|_| KernelError::InUse)?;

    log::info!(
        "virtio-net initialized: mac={:02x?} ipv4={}.{}.{}.{} gw={}.{}.{}.{}",
        mac,
        DEFAULT_IPV4_ADDR[0],
        DEFAULT_IPV4_ADDR[1],
        DEFAULT_IPV4_ADDR[2],
        DEFAULT_IPV4_ADDR[3],
        DEFAULT_IPV4_GATEWAY[0],
        DEFAULT_IPV4_GATEWAY[1],
        DEFAULT_IPV4_GATEWAY[2],
        DEFAULT_IPV4_GATEWAY[3],
    );

    Ok(())
}

pub fn poll_network() -> Result<(), KernelError> {
    with_net_core(|core| {
        let _ = core
            .iface
            .poll(smol_now(), &mut core.device, &mut core.sockets);
    })
}

pub fn wait_delay() -> Duration {
    with_net_core(|core| {
        core.iface
            .poll_delay(smol_now(), &core.sockets)
            .map(|delay| Duration::from_micros(delay.total_micros()))
            .unwrap_or(DEFAULT_WAIT)
    })
    .unwrap_or(DEFAULT_WAIT)
}

pub fn allocate_ephemeral_port() -> u16 {
    loop {
        let next = NEXT_EPHEMERAL_PORT.fetch_add(1, Ordering::Relaxed);
        let port = 49152 + (next % (65535 - 49152)) as u16;
        if port != 0 {
            return port;
        }
    }
}

pub fn process_packets() {
    let _ = poll_network();
    socket_wait_queue().lock_save_irq().wake_all();
}

pub fn handle_irq() {
    if let Some(hw) = NET_DEVICE.get() {
        let _ = hw.lock_save_irq().net.ack_interrupt();
    }
    process_packets();
}

#[repr(i32)]
pub enum ShutdownHow {
    Read = 0,
    Write = 1,
    ReadWrite = 2,
}

impl TryFrom<i32> for ShutdownHow {
    type Error = KernelError;
    fn try_from(value: i32) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(ShutdownHow::Read),
            1 => Ok(ShutdownHow::Write),
            2 => Ok(ShutdownHow::ReadWrite),
            _ => Err(KernelError::InvalidValue),
        }
    }
}

#[non_exhaustive]
#[derive(Debug, Clone)]
pub enum SockAddr {
    In(SockAddrIn),
    Un(SockAddrUn),
}

impl SockAddr {
    pub fn len(&self) -> SocketLen {
        match self {
            SockAddr::In(_) => size_of::<SockAddrIn>(),
            SockAddr::Un(_) => size_of::<SockAddrUn>(),
        }
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        match self {
            SockAddr::In(sain) => unsafe {
                core::slice::from_raw_parts(
                    (sain as *const SockAddrIn).cast::<u8>(),
                    size_of::<SockAddrIn>(),
                )
                .to_vec()
            },
            SockAddr::Un(saun) => unsafe {
                core::slice::from_raw_parts(
                    (saun as *const SockAddrUn).cast::<u8>(),
                    size_of::<SockAddrUn>(),
                )
                .to_vec()
            },
        }
    }
}

#[derive(Copy, Clone, Debug)]
#[repr(C, packed)]
pub struct SockAddrIn {
    family: u16,
    port: [u8; 2],
    addr: [u8; 4],
    zero: [u8; 8],
}

#[derive(Copy, Clone, Debug)]
#[repr(C, packed)]
pub struct SockAddrUn {
    family: u16,
    path: [u8; 108],
}

unsafe impl crate::memory::uaccess::UserCopyable for SockAddrIn {}
unsafe impl crate::memory::uaccess::UserCopyable for SockAddrUn {}

impl TryFrom<SockAddr> for IpEndpoint {
    type Error = KernelError;
    fn try_from(sockaddr: SockAddr) -> Result<IpEndpoint, KernelError> {
        match sockaddr {
            SockAddr::In(SockAddrIn { port, addr, .. }) => Ok(IpEndpoint {
                port: u16::from_be_bytes(port),
                addr: IpAddress::Ipv4(Ipv4Addr::from(addr)),
            }),
            _ => Err(KernelError::InvalidValue),
        }
    }
}

impl TryFrom<SockAddr> for IpListenEndpoint {
    type Error = KernelError;
    fn try_from(sockaddr: SockAddr) -> Result<IpListenEndpoint, KernelError> {
        match sockaddr {
            SockAddr::In(SockAddrIn { port, addr, .. }) => {
                let addr = Ipv4Addr::from(addr);
                Ok(IpListenEndpoint {
                    addr: (!addr.is_unspecified()).then_some(IpAddress::Ipv4(addr)),
                    port: u16::from_be_bytes(port),
                })
            }
            _ => Err(KernelError::InvalidValue),
        }
    }
}

impl From<IpEndpoint> for SockAddr {
    fn from(endpoint: IpEndpoint) -> SockAddr {
        SockAddr::In(SockAddrIn {
            family: AF_INET as u16,
            port: endpoint.port.to_be_bytes(),
            addr: match endpoint.addr {
                IpAddress::Ipv4(addr) => addr.octets(),
                _ => unimplemented!(),
            },
            zero: [0; 8],
        })
    }
}

pub async fn parse_sockaddr(uaddr: UA, len: SocketLen) -> Result<SockAddr, KernelError> {
    use crate::memory::uaccess::try_copy_from_user;
    use libkernel::memory::address::TUA;

    // Need at least a family field
    if len < size_of::<u16>() {
        return Err(KernelError::InvalidValue);
    }

    let family: u16 = copy_from_user(TUA::from_value(uaddr.value())).await?;

    match family as i32 {
        AF_INET => {
            if len < size_of::<SockAddrIn>() {
                return Err(KernelError::InvalidValue);
            }
            let sain: SockAddrIn = try_copy_from_user(uaddr.cast())?;
            Ok(SockAddr::In(sain))
        }
        AF_UNIX => {
            let path_len = len - size_of::<u16>() * 2;
            if path_len > 108 {
                return Err(KernelError::InvalidValue);
            }
            let mut path = [0u8; 108];
            copy_from_user_slice(uaddr.add_bytes(size_of::<u16>()), &mut path[..path_len]).await?;
            let saun: SockAddrUn = SockAddrUn { family, path };
            Ok(SockAddr::Un(saun))
        }
        _ => Err(KernelError::AddressFamilyNotSupported),
    }
}

pub async fn wait_for_network_progress() {
    crate::drivers::timer::sleep(wait_delay()).await;
}

pub fn tcp_socket_state(
    handle: smoltcp::iface::SocketHandle,
) -> Result<smol_tcp::State, KernelError> {
    with_net_core(|core| core.sockets.get::<smol_tcp::Socket>(handle).state())
}

pub fn tcp_socket_remote_endpoint(
    handle: smoltcp::iface::SocketHandle,
) -> Result<Option<IpEndpoint>, KernelError> {
    with_net_core(|core| {
        core.sockets
            .get::<smol_tcp::Socket>(handle)
            .remote_endpoint()
    })
}
