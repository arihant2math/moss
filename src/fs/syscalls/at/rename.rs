use core::ffi::c_char;
use libkernel::fs::path::Path;
use crate::process::fd_table::Fd;
use libkernel::memory::address::TUA;
use crate::fs::syscalls::at::resolve_at_start_node;
use crate::memory::uaccess::cstr::UserCStr;

pub async fn sys_renameat(
    old_dir_fd: Fd,
    old_path: TUA<c_char>,
    new_dir_fd: Fd,
    new_path: TUA<c_char>,
) -> libkernel::error::Result<usize> {
    let mut buf = [0u8; 1024];
    let old_path = Path::new(UserCStr::from_ptr(old_path).copy_from_user(&mut buf).await?);
    let mut buf = [0u8; 1024];
    let new_path = Path::new(UserCStr::from_ptr(new_path).copy_from_user(&mut buf).await?);
    let old_start_node = resolve_at_start_node(old_dir_fd, old_path).await?;
    let new_start_node = resolve_at_start_node(new_dir_fd, new_path).await?;
    
    Ok(0)
}
