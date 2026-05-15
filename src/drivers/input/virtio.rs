use crate::{
    arch::ArchImpl,
    drivers::{
        Driver, DriverManager,
        init::PlatformBus,
        input::{
            EV_ABS, EV_MAX, InputAbsInfo, InputDevice, InputDeviceInfo, InputId,
            register_input_device,
        },
        probe::{DeviceDescriptor, DeviceMatchType},
        virtio_hal::VirtioHal,
    },
    interrupts::{ClaimedInterrupt, InterruptHandler},
    kernel_driver,
    sync::SpinLock,
};
use alloc::{boxed::Box, collections::BTreeMap, format, sync::Arc, vec::Vec};
use core::ptr::NonNull;
use libkernel::{
    error::{KernelError, ProbeError, Result},
    memory::{
        address::{PA, VA},
        proc_vm::address_space::{KernAddressSpace, VirtualMemory},
        region::PhysMemoryRegion,
    },
};
use log::{info, warn};
use virtio_drivers::{
    device::input::{DevIDs, VirtIOInput},
    transport::{
        DeviceType, Transport,
        mmio::{MmioTransport, VirtIOHeader},
    },
};

pub struct VirtioInputDriver<T: Transport + Send> {
    fdt_name: Option<&'static str>,
    input: SpinLock<VirtIOInput<VirtioHal, T>>,
    input_device: Arc<InputDevice>,
    _interrupt: Option<ClaimedInterrupt>,
}

impl<T: Transport + Send> VirtioInputDriver<T> {
    fn new(
        fdt_name: Option<&'static str>,
        input: VirtIOInput<VirtioHal, T>,
        input_device: Arc<InputDevice>,
        interrupt: Option<ClaimedInterrupt>,
    ) -> Self {
        Self {
            fdt_name,
            input: SpinLock::new(input),
            input_device,
            _interrupt: interrupt,
        }
    }
}

impl<T: Transport + Send + Sync + 'static> Driver for VirtioInputDriver<T> {
    fn name(&self) -> &'static str {
        self.fdt_name.unwrap_or("virtio-input")
    }
}

impl<T: Transport + Send + Sync + 'static> InterruptHandler for VirtioInputDriver<T> {
    fn handle_irq(&self, _desc: crate::interrupts::InterruptDescriptor) {
        let pending_events = {
            let mut input = self.input.lock_save_irq();
            let _ = input.ack_interrupt();

            let mut events = Vec::new();
            while let Some(event) = input.pop_pending_event() {
                events.push(event);
            }
            events
        };

        for event in pending_events {
            self.input_device
                .emit(event.event_type, event.code, event.value as i32);
        }
    }
}

fn for_each_set_bit(bitmap: &[u8], mut f: impl FnMut(u16)) {
    for (byte_idx, &byte) in bitmap.iter().enumerate() {
        let mut bits = byte;
        while bits != 0 {
            let low_bit = bits.trailing_zeros() as usize;
            f((byte_idx * 8 + low_bit) as u16);
            bits &= !(1 << low_bit);
        }
    }
}

fn build_device_info<T: Transport>(
    input: &mut VirtIOInput<VirtioHal, T>,
    node_name: &'static str,
) -> InputDeviceInfo {
    let name = input.name().unwrap_or_else(|_| "virtio-input".into());
    let serial = input.serial_number().ok().filter(|s| !s.is_empty());
    let ids = input.ids().unwrap_or_else(|_| DevIDs::default());
    let properties = input
        .prop_bits()
        .map(|bits| bits.into_vec())
        .unwrap_or_default();

    let mut event_bits = BTreeMap::new();
    let mut abs_info = BTreeMap::new();

    for event_type in 0..=EV_MAX as u8 {
        let bits = input.ev_bits(event_type).unwrap_or_default();
        if bits.is_empty() {
            continue;
        }

        if event_type as u16 == EV_ABS {
            for_each_set_bit(&bits, |axis| {
                if let Ok(axis_info) = input.abs_info(axis as u8) {
                    abs_info.insert(
                        axis,
                        InputAbsInfo {
                            value: 0,
                            minimum: axis_info.min as i32,
                            maximum: axis_info.max as i32,
                            fuzz: axis_info.fuzz as i32,
                            flat: axis_info.flat as i32,
                            resolution: axis_info.res as i32,
                        },
                    );
                }
            });
        }

        event_bits.insert(event_type as u16, bits.into_vec());
    }

    InputDeviceInfo {
        name,
        phys: Some(format!("virtio/{node_name}")),
        uniq: serial,
        id: InputId {
            bustype: ids.bustype,
            vendor: ids.vendor,
            product: ids.product,
            version: ids.version,
        },
        properties,
        event_bits,
        abs_info,
    }
}

fn virtio_input_probe(dm: &mut DriverManager, d: DeviceDescriptor) -> Result<Arc<dyn Driver>> {
    match d {
        DeviceDescriptor::Fdt(fdt_node, _flags) => {
            let _ = crate::drivers::input::evdev_manager().ok_or(ProbeError::Deferred)?;

            let region = fdt_node
                .reg()
                .ok_or(ProbeError::NoReg)?
                .next()
                .ok_or(ProbeError::NoReg)?;
            let size = region.size.ok_or(ProbeError::NoRegSize)?;

            let mapped: VA =
                ArchImpl::kern_address_space()
                    .lock_save_irq()
                    .map_mmio(PhysMemoryRegion::new(
                        PA::from_value(region.address as usize),
                        size,
                    ))?;

            let header = NonNull::new(mapped.value() as *mut VirtIOHeader)
                .ok_or(KernelError::InvalidValue)?;

            let transport = unsafe {
                match MmioTransport::new(header, size) {
                    Ok(t) => t,
                    Err(_) => return Err(KernelError::Probe(ProbeError::NoMatch)),
                }
            };

            if !matches!(transport.device_type(), DeviceType::Input) {
                return Err(KernelError::Probe(ProbeError::NoMatch));
            }

            info!("virtio-input found at {mapped:?} (node {})", fdt_node.name);

            let mut input = VirtIOInput::<VirtioHal, _>::new(transport)
                .map_err(|_| KernelError::Other("virtio-input init failed"))?;
            let device_info = build_device_info(&mut input, fdt_node.name);
            let input_device = register_input_device(device_info)?;

            let driver: Arc<dyn Driver> = if let Some(interrupt_parent) =
                fdt_node.interrupt_parent()
            {
                if let Some(interrupt_manager) = dm
                    .find_by_name(interrupt_parent.node.name)
                    .and_then(|driver| driver.as_interrupt_manager())
                {
                    if let Some(mut interrupt) =
                        fdt_node.interrupts().and_then(|mut ints| ints.next())
                    {
                        let config = interrupt_manager.parse_fdt_interrupt_regs(&mut interrupt)?;
                        let irq_input_device = input_device.clone();

                        interrupt_manager.claim_interrupt(config, move |claimed_interrupt| {
                            VirtioInputDriver::new(
                                Some(fdt_node.name),
                                input,
                                irq_input_device,
                                Some(claimed_interrupt),
                            )
                        })?
                    } else {
                        warn!(
                            "virtio-input {} has no interrupt specifier; device will stay idle",
                            fdt_node.name,
                        );
                        Arc::new(VirtioInputDriver::new(
                            Some(fdt_node.name),
                            input,
                            input_device,
                            None,
                        ))
                    }
                } else {
                    warn!(
                        "virtio-input {} initialized without IRQ handler; device will stay idle",
                        fdt_node.name,
                    );
                    Arc::new(VirtioInputDriver::new(
                        Some(fdt_node.name),
                        input,
                        input_device,
                        None,
                    ))
                }
            } else {
                warn!(
                    "virtio-input {} initialized without interrupt parent; device will stay idle",
                    fdt_node.name,
                );
                Arc::new(VirtioInputDriver::new(
                    Some(fdt_node.name),
                    input,
                    input_device,
                    None,
                ))
            };

            Ok(driver)
        }
    }
}

pub fn virtio_input_init(bus: &mut PlatformBus, _dm: &mut DriverManager) -> Result<()> {
    bus.register_platform_driver(
        DeviceMatchType::FdtCompatible("virtio,mmio"),
        Box::new(virtio_input_probe),
    );

    bus.register_platform_driver(
        DeviceMatchType::FdtCompatible("virtio-mmio"),
        Box::new(virtio_input_probe),
    );

    Ok(())
}

kernel_driver!(virtio_input_init);
