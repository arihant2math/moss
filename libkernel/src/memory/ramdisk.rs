//! Ramdisks that have been mapped into the kernel's address space.

use crate::memory::address::TVA;
use core::slice;

#[cfg(feature = "proc_vm")]
use crate::{
    error::{MapError, Result},
    memory::{
        address::VA,
        paging::permissions::PtePermissions,
        proc_vm::address_space::KernAddressSpace,
        region::{PhysMemoryRegion, VirtMemoryRegion},
    },
};

/// Our representation of ramdisks in the kernel's address space
///
/// other ideas were:
/// `'static &[u8]` and good docs :) which would have just been dumb
/// or simply tossing around raw (ptr*, len)s, which is also stupid...
#[derive(Debug)]
pub struct Ramdisk {
    ptr: TVA<u8>,
    len: usize,
}

impl Ramdisk {
    /// Create a ramdisk.
    /// note that the segment oof memory we create it on
    /// must already ahve been mapped
    /// or we will get UB.
    pub unsafe fn from_raw(ptr: TVA<u8>, len: usize) -> Self {
        Self { ptr, len }
    }

    /// Give the actual contents of the ramdisk as a (reference to a) slice.
    pub fn as_bytes(&self) -> &[u8] {
        unsafe { slice::from_raw_parts(self.ptr.as_ptr(), self.len) }
    }

    /// virtual addr of the start ptr
    pub fn base(&self) -> TVA<u8> {
        self.ptr
    }

    /// why
    pub fn len(&self) -> usize {
        self.len
    }
}

#[cfg(feature = "proc_vm")]
impl Ramdisk {
    /// Map `region` into the kernel's address space at `base`.
    /// this coveres the page-aligned segment of memory contianing `region`.
    pub fn map<K: KernAddressSpace>(
        region: PhysMemoryRegion,
        base: VA,
        kern_addr_spc: &mut K,
    ) -> Result<Self> {
        if !region.is_page_aligned() {
            return Err(MapError::PhysNotAligned.into());
        }

        if !base.is_page_aligned() {
            return Err(MapError::VirtNotAligned.into());
        }

        let mapped = region.align_to_page_boundary();

        kern_addr_spc.map_normal(
            mapped,
            VirtMemoryRegion::new(base, mapped.size()),
            PtePermissions::rw(false),
        )?;

        Ok(unsafe { Self::from_raw(base.cast(), region.size()) })
    }
}
