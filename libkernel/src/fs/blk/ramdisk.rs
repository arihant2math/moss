//! RAM-backed block device implementation.

use crate::{
    error::{IoError, KernelError, Result},
    fs::BlockDevice,
    memory::{PAGE_SIZE, ramdisk::Ramdisk},
};
use alloc::boxed::Box;
use async_trait::async_trait;
use core::ptr;

/// A block device backed by a region of RAM.
pub struct RamdiskBlkDev {
    rd: Ramdisk,
    num_blocks: u64,
}

const BLOCK_SIZE: usize = PAGE_SIZE;

impl RamdiskBlkDev {
    /// Take in a ramdisk (reference struct) and create a block device over it
    pub fn new(rd: Ramdisk) -> Result<Self> {
        if !rd.len().is_multiple_of(BLOCK_SIZE) {
            return Err(KernelError::InvalidValue);
        }

        let num_blocks = (rd.len() / BLOCK_SIZE) as u64;

        Ok(Self { rd, num_blocks })
    }
}

#[async_trait]
impl BlockDevice for RamdiskBlkDev {
    /// Read one or more blocks starting at `block_id`.
    /// The `buf` length must be a multiple of `block_size`.
    async fn read(&self, block_id: u64, buf: &mut [u8]) -> Result<()> {
        debug_assert!(buf.len().is_multiple_of(BLOCK_SIZE));

        let num_blocks_to_read = (buf.len() / BLOCK_SIZE) as u64;

        // Ensure the read doesn't go past the end of the ramdisk.
        if block_id + num_blocks_to_read > self.num_blocks {
            return Err(IoError::OutOfBounds.into());
        }

        let offset = block_id as usize * BLOCK_SIZE;

        unsafe {
            // SAFETY: VA can be accessed:
            //
            // 1. We have successfully mapped the ramdisk into virtual memory,
            //    starting at base.
            // 2. We have bounds checked the access.
            let src_ptr = self.rd.base().as_ptr().add(offset);

            ptr::copy_nonoverlapping(src_ptr, buf.as_mut_ptr(), buf.len());
        }

        Ok(())
    }

    /// Write one or more blocks starting at `block_id`.
    /// The `buf` length must be a multiple of `block_size`.
    async fn write(&self, block_id: u64, buf: &[u8]) -> Result<()> {
        debug_assert!(buf.len().is_multiple_of(BLOCK_SIZE));

        let num_blocks_to_write = (buf.len() / BLOCK_SIZE) as u64;

        if block_id + num_blocks_to_write > self.num_blocks {
            return Err(IoError::OutOfBounds.into());
        }

        let offset = block_id as usize * BLOCK_SIZE;

        unsafe {
            let dest_ptr = self.rd.base().as_ptr_mut().add(offset);

            ptr::copy_nonoverlapping(buf.as_ptr(), dest_ptr, buf.len());
        }

        Ok(())
    }

    /// The size of a single block in bytes.
    fn block_size(&self) -> usize {
        BLOCK_SIZE
    }

    /// Flushes any caches to the underlying device.
    async fn sync(&self) -> Result<()> {
        Ok(())
    }
}
