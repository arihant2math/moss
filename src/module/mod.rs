use core::ffi::c_char;
use log::error;
use paste::paste;

///! Linux module support

type ModuleInitFunc = unsafe extern "C" fn() -> i32;

macro_rules! run_initcall_section {
    ($i: ident) => {
        paste! {
            unsafe fn [<$i _section>]() {
                unsafe extern "C" {
                    static [<__ $i _start>]: ModuleInitFunc;
                    static [<__ $i _end>]: ModuleInitFunc;
                }

                // SAFETY: The linker script defines `__initcallX_start/end` as
                // the bounds of a contiguous array of `ModuleInitFunc`.
                unsafe {
                    let start = core::ptr::addr_of!([<__ $i _start>]);
                    let end = core::ptr::addr_of!([<__ $i _end>]);

                    // If these ever end up misaligned, slice creation would be UB.
                    assert_eq!((start as usize) % core::mem::align_of::<ModuleInitFunc>(), 0);
                    assert_eq!((end as usize) % core::mem::align_of::<ModuleInitFunc>(), 0);

                    run_initcall_section_fns(stringify!($i), start, end);
                }
            }
        }
    };
}

unsafe fn run_initcall_section_fns(
    level: &'static str,
    start: *const ModuleInitFunc,
    end: *const ModuleInitFunc,
) {
    let count = unsafe { end.offset_from(start) as usize };
    let slice = unsafe { core::slice::from_raw_parts(start, count) };

    log::info!("Running {count} initcalls for {level}");
    for &f in slice {
        let ret = unsafe { f() };
        if ret != 0 {
            error!("Module init function returned error code: {}", ret);
        }
    }
}

run_initcall_section!(initcall0);
run_initcall_section!(initcall1);
run_initcall_section!(initcall2);
run_initcall_section!(initcall3);
run_initcall_section!(initcall4);
run_initcall_section!(initcall5);
run_initcall_section!(initcall6);
run_initcall_section!(initcall7);

pub fn do_initcalls() {
    unsafe {
        initcall0_section();
        initcall1_section();
        initcall2_section();
        initcall3_section();
        initcall4_section();
        initcall5_section();
        initcall6_section();
        initcall7_section();
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn printk(format: *const c_char, _args: ...) {
    // TODO: handle args
    let format_str = unsafe { core::ffi::CStr::from_ptr(format) };
    let log_level = if format_str.to_bytes()[0] == b'\x01' {
        Some(match format_str.to_bytes()[1] {
            0 => log::Level::Error,
            1 => log::Level::Warn,
            2 => log::Level::Info,
            3 => log::Level::Debug,
            4 => log::Level::Trace,
            _ => {
                error!("Invalid log level in printk format string: {}", format_str.to_string_lossy());
                log::Level::Info
            }
        })
    } else {
        None
    };
    log::log!(
        log_level.unwrap_or(log::Level::Info),
        "{}",
        format_str.to_string_lossy()
    );
}
