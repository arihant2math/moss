use core::{
    future::Future,
    mem::size_of,
    pin::Pin,
    sync::atomic::{AtomicU64, Ordering},
};

use alloc::{
    boxed::Box,
    collections::{BTreeMap, BTreeSet, VecDeque},
    format,
    string::String,
    sync::Arc,
    vec,
    vec::Vec,
};
use async_trait::async_trait;
use libkernel::{
    driver::CharDevDescriptor,
    error::{KernelError, ProbeError, Result},
    fs::{OpenFlags, SeekFrom, attr::FilePermissions, path::Path},
    memory::address::{TUA, UA},
    sync::condvar::WakeupType,
};

use crate::{
    drivers::timer::uptime,
    drivers::{
        CharDriver, DriverManager, OpenableDevice, ReservedMajors, fs::dev::devfs,
        init::PlatformBus,
    },
    fs::{
        fops::FileOps,
        open_file::{FileCtx, OpenFile},
    },
    kernel_driver,
    memory::uaccess::{UserCopyable, copy_objs_to_user, copy_to_user, copy_to_user_slice},
    process::thread_group::signal::{InterruptResult, Interruptable},
    sync::{CondVar, OnceLock, SpinLock},
};

pub mod virtio;

pub const EV_VERSION: u32 = 0x010001;
pub const INPUT_EVENT_MINOR_BASE: u64 = 64;

pub const EV_SYN: u16 = 0x00;
pub const EV_KEY: u16 = 0x01;
#[expect(dead_code)]
pub const EV_REL: u16 = 0x02;
pub const EV_ABS: u16 = 0x03;
pub const EV_MAX: u16 = 0x1f;

pub const SYN_REPORT: u16 = 0x00;
pub const SYN_DROPPED: u16 = 0x03;

const EVDEV_IOCTL_TYPE: usize = b'E' as usize;
const IOC_NRBITS: usize = 8;
const IOC_TYPEBITS: usize = 8;
const IOC_SIZEBITS: usize = 14;
const IOC_NRSHIFT: usize = 0;
const IOC_TYPESHIFT: usize = IOC_NRSHIFT + IOC_NRBITS;
const IOC_SIZESHIFT: usize = IOC_TYPESHIFT + IOC_TYPEBITS;
const EVDEV_QUEUE_LIMIT: usize = 256;

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct TimeVal {
    pub tv_sec: i64,
    pub tv_usec: i64,
}

unsafe impl UserCopyable for TimeVal {}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct InputEvent {
    pub time: TimeVal,
    pub type_: u16,
    pub code: u16,
    pub value: i32,
}

impl InputEvent {
    fn new(event_type: u16, code: u16, value: i32) -> Self {
        Self {
            time: current_input_time(),
            type_: event_type,
            code,
            value,
        }
    }

    fn syn_dropped() -> Self {
        Self::new(EV_SYN, SYN_DROPPED, 0)
    }
}

unsafe impl UserCopyable for InputEvent {}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct InputId {
    pub bustype: u16,
    pub vendor: u16,
    pub product: u16,
    pub version: u16,
}

unsafe impl UserCopyable for InputId {}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct InputAbsInfo {
    pub value: i32,
    pub minimum: i32,
    pub maximum: i32,
    pub fuzz: i32,
    pub flat: i32,
    pub resolution: i32,
}

unsafe impl UserCopyable for InputAbsInfo {}

#[derive(Debug, Default)]
pub struct InputDeviceInfo {
    pub name: String,
    pub phys: Option<String>,
    pub uniq: Option<String>,
    pub id: InputId,
    pub properties: Vec<u8>,
    pub event_bits: BTreeMap<u16, Vec<u8>>,
    pub abs_info: BTreeMap<u16, InputAbsInfo>,
}

pub fn set_bitmap_bit(bitmap: &mut Vec<u8>, bit: u16) {
    let byte = (bit as usize) / 8;
    let bit_in_byte = (bit as usize) % 8;

    if bitmap.len() <= byte {
        bitmap.resize(byte + 1, 0);
    }

    bitmap[byte] |= 1 << bit_in_byte;
}

fn current_input_time() -> TimeVal {
    let now = uptime();

    TimeVal {
        tv_sec: now.as_secs() as i64,
        tv_usec: now.subsec_micros() as i64,
    }
}

fn ioc_type(request: usize) -> usize {
    (request >> IOC_TYPESHIFT) & ((1 << IOC_TYPEBITS) - 1)
}

fn ioc_nr(request: usize) -> usize {
    (request >> IOC_NRSHIFT) & ((1 << IOC_NRBITS) - 1)
}

fn ioc_size(request: usize) -> usize {
    (request >> IOC_SIZESHIFT) & ((1 << IOC_SIZEBITS) - 1)
}

fn pop_events(queue: &mut VecDeque<InputEvent>, max_events: usize) -> Option<Vec<InputEvent>> {
    if queue.is_empty() {
        return None;
    }

    let count = core::cmp::min(max_events, queue.len());
    let mut out = Vec::with_capacity(count);

    for _ in 0..count {
        if let Some(event) = queue.pop_front() {
            out.push(event);
        }
    }

    Some(out)
}

fn fill_bytes(dst_len: usize, src: &[u8]) -> Vec<u8> {
    let mut out = vec![0; dst_len];
    let count = core::cmp::min(dst_len, src.len());
    out[..count].copy_from_slice(&src[..count]);
    out
}

fn fill_string(dst_len: usize, value: Option<&str>) -> Vec<u8> {
    let mut out = vec![0; dst_len];

    if let Some(value) = value {
        let value = value.as_bytes();
        let count = core::cmp::min(dst_len, value.len());
        out[..count].copy_from_slice(&value[..count]);
    }

    out
}

struct EvdevClient {
    queue: CondVar<VecDeque<InputEvent>>,
}

impl EvdevClient {
    fn new() -> Self {
        Self {
            queue: CondVar::new(VecDeque::new()),
        }
    }

    fn push(&self, event: InputEvent) {
        self.queue.update(|queue| {
            if queue.len() >= EVDEV_QUEUE_LIMIT {
                queue.clear();
                queue.push_back(InputEvent::syn_dropped());
            }

            queue.push_back(event);
            WakeupType::All
        });
    }

    fn try_read(&self, max_events: usize) -> Option<Vec<InputEvent>> {
        let mut out = None;

        self.queue.update(|queue| {
            out = pop_events(queue, max_events);
            WakeupType::None
        });

        out
    }

    async fn read(&self, max_events: usize) -> Vec<InputEvent> {
        self.queue
            .wait_until(|queue| pop_events(queue, max_events))
            .await
    }

    async fn wait_read_ready(&self) {
        self.queue
            .wait_until(|queue| if queue.is_empty() { None } else { Some(()) })
            .await;
    }
}

struct InputDeviceState {
    next_client_id: u64,
    clients: BTreeMap<u64, Arc<EvdevClient>>,
    key_state: BTreeSet<u16>,
    abs_info: BTreeMap<u16, InputAbsInfo>,
}

pub struct InputDevice {
    descriptor: CharDevDescriptor,
    event_index: u64,
    name: String,
    phys: Option<String>,
    uniq: Option<String>,
    id: InputId,
    properties: Box<[u8]>,
    event_bits: BTreeMap<u16, Box<[u8]>>,
    state: SpinLock<InputDeviceState>,
}

impl InputDevice {
    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn emit(&self, event_type: u16, code: u16, value: i32) {
        let event = InputEvent::new(event_type, code, value);

        let clients = {
            let mut state = self.state.lock_save_irq();

            match event_type {
                EV_KEY => {
                    if value == 0 {
                        state.key_state.remove(&code);
                    } else {
                        state.key_state.insert(code);
                    }
                }
                EV_ABS => {
                    if let Some(info) = state.abs_info.get_mut(&code) {
                        info.value = value;
                    }
                }
                _ => {}
            }

            state.clients.values().cloned().collect::<Vec<_>>()
        };

        for client in clients {
            client.push(event);
        }
    }

    fn register_client(&self) -> (u64, Arc<EvdevClient>) {
        let mut state = self.state.lock_save_irq();
        let client_id = state.next_client_id;
        state.next_client_id += 1;

        let client = Arc::new(EvdevClient::new());
        state.clients.insert(client_id, client.clone());

        (client_id, client)
    }

    fn unregister_client(&self, client_id: u64) {
        self.state.lock_save_irq().clients.remove(&client_id);
    }

    fn bitmap_for_event_type(&self, event_type: u16, len: usize) -> Vec<u8> {
        if event_type == 0 {
            let mut out = vec![0; len];
            set_bitmap_bit(&mut out, EV_SYN);

            for &supported_type in self.event_bits.keys() {
                set_bitmap_bit(&mut out, supported_type);
            }

            return out;
        }

        if event_type == EV_SYN {
            let mut syn = Vec::new();
            set_bitmap_bit(&mut syn, SYN_REPORT);
            set_bitmap_bit(&mut syn, SYN_DROPPED);
            return fill_bytes(len, &syn);
        }

        self.event_bits
            .get(&event_type)
            .map(|bits| fill_bytes(len, bits))
            .unwrap_or_else(|| vec![0; len])
    }

    fn key_state_bitmap(&self, len: usize) -> Vec<u8> {
        let mut out = vec![0; len];
        let state = self.state.lock_save_irq();

        for &key in &state.key_state {
            set_bitmap_bit(&mut out, key);
        }

        out
    }

    fn abs_info(&self, axis: u16) -> Option<InputAbsInfo> {
        self.state.lock_save_irq().abs_info.get(&axis).copied()
    }
}

struct InputDeviceOpenable {
    device: Arc<InputDevice>,
}

impl OpenableDevice for InputDeviceOpenable {
    fn open(&self, flags: OpenFlags) -> Result<Arc<OpenFile>> {
        let (client_id, client) = self.device.register_client();

        Ok(Arc::new(OpenFile::new(
            Box::new(EvdevFile {
                device: self.device.clone(),
                client,
                client_id,
            }),
            flags,
        )))
    }
}

struct EvdevFile {
    device: Arc<InputDevice>,
    client: Arc<EvdevClient>,
    client_id: u64,
}

impl EvdevFile {
    async fn read_impl(&self, buf: UA, count: usize, nonblock: bool) -> Result<usize> {
        let event_size = size_of::<InputEvent>();
        let max_events = count / event_size;

        if max_events == 0 {
            return Err(KernelError::InvalidValue);
        }

        let events = if let Some(events) = self.client.try_read(max_events) {
            events
        } else if nonblock {
            return Err(KernelError::TryAgain);
        } else {
            match self.client.read(max_events).interruptable().await {
                InterruptResult::Interrupted => return Err(KernelError::Interrupted),
                InterruptResult::Uninterrupted(events) => events,
            }
        };

        copy_objs_to_user(&events, buf.cast()).await?;

        Ok(events.len() * event_size)
    }

    async fn copy_bitmap_ioctl(&self, argp: usize, size: usize, bitmap: &[u8]) -> Result<usize> {
        if size != 0 {
            copy_to_user_slice(&fill_bytes(size, bitmap), UA::from_value(argp)).await?;
        }

        Ok(0)
    }
}

#[async_trait]
impl FileOps for EvdevFile {
    async fn read(&mut self, ctx: &mut FileCtx, buf: UA, count: usize) -> Result<usize> {
        self.read_impl(buf, count, ctx.flags.contains(OpenFlags::O_NONBLOCK))
            .await
    }

    async fn readat(&mut self, buf: UA, count: usize, _offset: u64) -> Result<usize> {
        self.read_impl(buf, count, false).await
    }

    async fn writeat(&mut self, _buf: UA, _count: usize, _offset: u64) -> Result<usize> {
        Err(KernelError::InvalidValue)
    }

    fn poll_read_ready(&self) -> Pin<Box<dyn Future<Output = Result<()>> + 'static + Send>> {
        let client = self.client.clone();

        Box::pin(async move {
            let _ = client.wait_read_ready().interruptable().await;
            Ok(())
        })
    }

    async fn ioctl(&mut self, _ctx: &mut FileCtx, request: usize, argp: usize) -> Result<usize> {
        if ioc_type(request) != EVDEV_IOCTL_TYPE {
            return Err(KernelError::NotATty);
        }

        let nr = ioc_nr(request);
        let size = ioc_size(request);

        match nr {
            0x01 if size == size_of::<u32>() => {
                copy_to_user(TUA::from_value(argp), EV_VERSION).await?;
                Ok(0)
            }
            0x02 if size == size_of::<InputId>() => {
                copy_to_user(TUA::from_value(argp), self.device.id).await?;
                Ok(0)
            }
            0x06 => {
                if size != 0 {
                    let name = fill_string(size, Some(self.device.name.as_str()));
                    copy_to_user_slice(&name, UA::from_value(argp)).await?;
                }
                Ok(0)
            }
            0x07 => {
                if size != 0 {
                    let phys = fill_string(size, self.device.phys.as_deref());
                    copy_to_user_slice(&phys, UA::from_value(argp)).await?;
                }
                Ok(0)
            }
            0x08 => {
                if size != 0 {
                    let uniq = fill_string(size, self.device.uniq.as_deref());
                    copy_to_user_slice(&uniq, UA::from_value(argp)).await?;
                }
                Ok(0)
            }
            0x09 => {
                self.copy_bitmap_ioctl(argp, size, &self.device.properties)
                    .await
            }
            0x18 => {
                let bitmap = self.device.key_state_bitmap(size);
                self.copy_bitmap_ioctl(argp, size, &bitmap).await
            }
            0x20..=0x3f => {
                let event_type = (nr - 0x20) as u16;
                let bitmap = self.device.bitmap_for_event_type(event_type, size);
                self.copy_bitmap_ioctl(argp, size, &bitmap).await
            }
            0x40..=0x7f if size == size_of::<InputAbsInfo>() => {
                let axis = (nr - 0x40) as u16;
                let info = self
                    .device
                    .abs_info(axis)
                    .ok_or(KernelError::InvalidValue)?;
                copy_to_user(TUA::from_value(argp), info).await?;
                Ok(0)
            }
            _ => Err(KernelError::NotATty),
        }
    }

    async fn seek(&mut self, _ctx: &mut FileCtx, _pos: SeekFrom) -> Result<u64> {
        Err(KernelError::SeekPipe)
    }

    async fn release(&mut self, _ctx: &FileCtx) -> Result<()> {
        self.device.unregister_client(self.client_id);
        Ok(())
    }
}

pub struct EvdevManager {
    next_event_index: AtomicU64,
    devices: SpinLock<BTreeMap<u64, Arc<dyn OpenableDevice>>>,
}

impl EvdevManager {
    fn register_device(&self, mut info: InputDeviceInfo) -> Result<Arc<InputDevice>> {
        let event_index = self.next_event_index.fetch_add(1, Ordering::SeqCst);
        let minor = INPUT_EVENT_MINOR_BASE + event_index;
        let descriptor = CharDevDescriptor {
            major: ReservedMajors::Input as _,
            minor,
        };

        let syn_bits = info.event_bits.entry(EV_SYN).or_default();
        set_bitmap_bit(syn_bits, SYN_REPORT);
        set_bitmap_bit(syn_bits, SYN_DROPPED);

        let path = format!("input/event{event_index}");
        let device = Arc::new(InputDevice {
            descriptor,
            event_index,
            name: info.name,
            phys: info.phys,
            uniq: info.uniq,
            id: info.id,
            properties: info.properties.into_boxed_slice(),
            event_bits: info
                .event_bits
                .into_iter()
                .map(|(event_type, bitmap)| (event_type, bitmap.into_boxed_slice()))
                .collect(),
            state: SpinLock::new(InputDeviceState {
                next_client_id: 0,
                clients: BTreeMap::new(),
                key_state: BTreeSet::new(),
                abs_info: info.abs_info,
            }),
        });

        let openable: Arc<dyn OpenableDevice> = Arc::new(InputDeviceOpenable {
            device: device.clone(),
        });

        {
            let mut devices = self.devices.lock_save_irq();
            if devices.insert(minor, openable).is_some() {
                return Err(KernelError::InUse);
            }
        }

        if let Err(e) = devfs().mknod_path(
            Path::new(&path),
            descriptor,
            FilePermissions::from_bits_retain(0o600),
        ) {
            self.devices.lock_save_irq().remove(&minor);
            return Err(e);
        }

        log::info!(
            "registered input device {} as /dev/input/event{} ({:?})",
            device.name(),
            device.event_index,
            device.descriptor,
        );

        Ok(device)
    }
}

impl CharDriver for EvdevManager {
    fn get_device(&self, minor: u64) -> Option<Arc<dyn OpenableDevice>> {
        self.devices.lock_save_irq().get(&minor).cloned()
    }
}

static EVDEV_MANAGER: OnceLock<Arc<EvdevManager>> = OnceLock::new();

pub fn register_input_device(info: InputDeviceInfo) -> Result<Arc<InputDevice>> {
    EVDEV_MANAGER
        .get()
        .cloned()
        .ok_or(KernelError::Probe(ProbeError::Deferred))?
        .register_device(info)
}

pub fn evdev_manager() -> Option<Arc<EvdevManager>> {
    EVDEV_MANAGER.get().cloned()
}

pub fn evdev_init(_bus: &mut PlatformBus, dm: &mut DriverManager) -> Result<()> {
    devfs().mkdir(Path::new("input"), FilePermissions::from_bits_retain(0o755))?;

    let manager = Arc::new(EvdevManager {
        next_event_index: AtomicU64::new(0),
        devices: SpinLock::new(BTreeMap::new()),
    });

    EVDEV_MANAGER
        .set(manager.clone())
        .map_err(|_| KernelError::InUse)?;

    dm.register_char_driver(ReservedMajors::Input as _, manager)
}

kernel_driver!(evdev_init);
