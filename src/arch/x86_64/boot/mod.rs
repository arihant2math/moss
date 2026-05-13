use alloc::string::String;
use core::arch::global_asm;
use libkernel::CpuOps;

use super::X86_64;
use crate::kmain;

global_asm!(include_str!("start.s"));

/// Early x86_64 boot stage.
///
/// For now this mirrors the structure of the arm64 bootstrap entry and gives
/// the assembly entry point a Rust handoff target. Once the x86_64 port grows
/// real paging and bootloader parsing, this is where the early bootstrap state
/// should be plumbed through.
#[unsafe(no_mangle)]
extern "C" fn arch_init_stage1(
    _boot_arg0: usize,
    _boot_arg1: usize,
    _image_start: usize,
    _image_end: usize,
    _init_pages_start: usize,
) -> usize {
    unsafe extern "C" {
        static __boot_stack: u8;
    }

    core::ptr::addr_of!(__boot_stack).addr()
}

/// Secondary x86_64 boot stage.
///
/// The x86_64 port does not yet have a full exception-return path, so we only
/// hand control to `kmain()` and return to the assembly stub, which parks the
/// CPU afterwards.
#[unsafe(no_mangle)]
extern "C" fn arch_init_stage2(frame: *mut u8) -> *mut u8 {
    kmain(String::new(), frame);
    frame
}

#[unsafe(no_mangle)]
pub extern "C" fn park_cpu() -> ! {
    X86_64::halt()
}
