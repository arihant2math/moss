use crate::{fs::VFS, memory::uaccess::cstr::UserCStr, sched::current_task};
use core::ffi::c_char;
use libkernel::{error::Result, fs::path::Path, memory::address::TUA};

pub async fn sys_chdir(path: TUA<c_char>) -> Result<usize> {
    let mut buf = [0; 1024];

    let path = Path::new(UserCStr::from_ptr(path).copy_from_user(&mut buf).await?);
    let task = current_task();
    let current_path = task.cwd.lock_save_irq().clone();

    let node = VFS.resolve_path(path, current_path).await?;

    *task.cwd.lock_save_irq() = node;

    Ok(0)
}
