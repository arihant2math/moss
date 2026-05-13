use crate::{
    arch::ArchImpl,
    drivers::{
        Driver, DriverManager,
        init::PlatformBus,
        probe::{DeviceDescriptor, DeviceMatchType},
    },
    kernel_driver, net,
};
use alloc::{boxed::Box, sync::Arc};
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
use virtio_drivers::transport::{
    DeviceType, Transport,
    mmio::{MmioTransport, VirtIOHeader},
};

pub struct VirtioNetDriver {
    fdt_name: Option<&'static str>,
    _interrupt: Option<crate::interrupts::ClaimedInterrupt>,
}

impl Driver for VirtioNetDriver {
    fn name(&self) -> &'static str {
        self.fdt_name.unwrap_or("virtio-net")
    }
}

impl crate::interrupts::InterruptHandler for VirtioNetDriver {
    fn handle_irq(&self, _desc: crate::interrupts::InterruptDescriptor) {
        net::handle_irq();
    }
}

fn virtio_net_probe(dm: &mut DriverManager, d: DeviceDescriptor) -> Result<Arc<dyn Driver>> {
    match d {
        DeviceDescriptor::Fdt(fdt_node, _flags) => {
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

            if !matches!(transport.device_type(), DeviceType::Network) {
                return Err(KernelError::Probe(ProbeError::NoMatch));
            }

            info!("virtio-net found at {mapped:?} (node {})", fdt_node.name);
            net::init_virtio_net(transport)?;

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
                        interrupt_manager.claim_interrupt(config, |claimed_interrupt| {
                            VirtioNetDriver {
                                fdt_name: Some(fdt_node.name),
                                _interrupt: Some(claimed_interrupt),
                            }
                        })?
                    } else {
                        Arc::new(VirtioNetDriver {
                            fdt_name: Some(fdt_node.name),
                            _interrupt: None,
                        })
                    }
                } else {
                    warn!(
                        "virtio-net {} initialized without IRQ handler; polling fallback only",
                        fdt_node.name,
                    );
                    Arc::new(VirtioNetDriver {
                        fdt_name: Some(fdt_node.name),
                        _interrupt: None,
                    })
                }
            } else {
                Arc::new(VirtioNetDriver {
                    fdt_name: Some(fdt_node.name),
                    _interrupt: None,
                })
            };

            Ok(driver)
        }
    }
}

pub fn virtio_net_init(bus: &mut PlatformBus, _dm: &mut DriverManager) -> Result<()> {
    bus.register_platform_driver(
        DeviceMatchType::FdtCompatible("virtio,mmio"),
        Box::new(virtio_net_probe),
    );

    bus.register_platform_driver(
        DeviceMatchType::FdtCompatible("virtio-mmio"),
        Box::new(virtio_net_probe),
    );

    Ok(())
}

kernel_driver!(virtio_net_init);
