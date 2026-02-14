#![no_std]

#![feature(used_with_arg)]

unsafe extern "C" {
    fn moss_test(i: i32) -> i32;
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn init_simple_module() -> i32 {
    unsafe { moss_test(42) }
}

#[unsafe(link_section = ".initcall6")]
#[used(linker)]
static INIT_SIMPLE_MODULE: unsafe extern "C" fn() -> i32 = init_simple_module;
