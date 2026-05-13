use core::{arch::asm, arch::x86_64::__cpuid};

use libkernel::CpuOps;

mod boot;
pub mod memory;

#[allow(non_camel_case_types)]
pub struct X86_64;

impl CpuOps for X86_64 {
    type InterruptFlags = u64;

    fn id() -> usize {
        // CPUID leaf 1 EBX[31:24] holds the initial local APIC ID on x86_64.
        ((__cpuid(1).ebx >> 24) & 0xff) as usize
    }

    fn halt() -> ! {
        loop {
            unsafe {
                asm!("hlt", options(nomem, nostack));
            }
        }
    }

    fn disable_interrupts() -> Self::InterruptFlags {
        let flags: u64;
        unsafe {
            asm!(
                "pushfq",
                "pop {}",
                "cli",
                out(reg) flags,
                options(nomem)
            );
        }
        flags
    }

    fn restore_interrupt_state(flags: Self::InterruptFlags) {
        unsafe {
            asm!(
                "push {}",
                "popfq",
                in(reg) flags,
                options(nomem)
            );
        }
    }

    fn enable_interrupts() {
        unsafe {
            asm!("sti", options(nomem, nostack));
        }
    }
}
