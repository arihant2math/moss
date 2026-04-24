use alloc::collections::{BTreeMap, btree_map::Entry};
use libkernel::{
    error::Result,
    memory::{page::PageFrame, proc_vm::vmarea::SharedAnonBacking},
};

use crate::sync::SpinLock;

use super::{PAGE_ALLOC, page::ClaimedPage};

/// Lazily allocated pages shared by all mappings of an anonymous backing.
///
/// The backing retains one canonical reference per page until its last mapping
/// is dropped. Each fault obtains an additional reference for its page table.
pub struct SharedAnon {
    pages: SpinLock<BTreeMap<usize, ClaimedPage>>,
}

impl SharedAnon {
    pub fn new() -> Self {
        Self {
            pages: SpinLock::new(BTreeMap::new()),
        }
    }
}

impl Default for SharedAnon {
    fn default() -> Self {
        Self::new()
    }
}

impl SharedAnonBacking for SharedAnon {
    fn claim_page(&self, offset: usize) -> Result<PageFrame> {
        // Faults hold the VM lock first: VM -> backing -> page allocator.
        let mut pages = self.pages.lock_save_irq();
        let page = match pages.entry(offset) {
            Entry::Occupied(entry) => entry.into_mut(),
            Entry::Vacant(entry) => entry.insert(ClaimedPage::alloc_zeroed()?),
        };

        // SAFETY: The canonical page is allocated and held alive under the
        // backing lock. This temporary allocation aliases its ownership, so it
        // must be leaked rather than dropped. Only clone() adds a reference;
        // that extra owned reference is transferred to the caller.
        let allocation = unsafe {
            PAGE_ALLOC
                .get()
                .unwrap()
                .alloc_from_region(page.pa().to_pfn().as_phys_range())
        };
        let claimed = allocation.clone().leak();
        allocation.leak();

        Ok(claimed.start_address().to_pfn())
    }
}
