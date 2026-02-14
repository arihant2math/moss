#![no_std]

#![feature(used_with_arg)]

use core::ffi::c_char;

unsafe extern "C" {
    fn printk(format: *const c_char, ...);
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn init_simple_module() -> i32 {
    unsafe {
        printk(c"Hello from simple rust module!".as_ptr());
    }
    0
}

#[unsafe(link_section = ".initcall6")]
#[used(linker)]
static INIT_SIMPLE_MODULE: unsafe extern "C" fn() -> i32 = init_simple_module;
