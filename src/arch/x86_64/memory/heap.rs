use super::super::X86_64;
use super::{PageOffsetTranslator, PgAllocGetter};
use core::{
    ops::{Deref, DerefMut},
    ptr,
    sync::atomic::{AtomicPtr, Ordering},
};
use libkernel::{
    CpuOps,
    memory::{
        allocators::slab::{
            allocator::SlabAllocator,
            cache::SlabCache,
            heap::{KHeap, SlabCacheStorage, SlabGetter},
        },
        claimed_page::ClaimedPage,
    },
    sync::once_lock::OnceLock,
};

const MAX_CPUS: usize = 256;

type SlabAlloc = SlabAllocator<X86_64, PgAllocGetter, PageOffsetTranslator>;

pub static SLAB_ALLOC: OnceLock<SlabAlloc, X86_64> = OnceLock::new();

pub struct StaticSlabGetter;

impl SlabGetter<X86_64, PgAllocGetter, PageOffsetTranslator> for StaticSlabGetter {
    fn global_slab_alloc() -> &'static SlabAlloc {
        SLAB_ALLOC.get().unwrap()
    }
}

// TODO: Replace this bootstrap cache registry with proper GS-based per-CPU
// storage once the x86_64 port has local CPU state setup.
static PER_CPU_SLAB_CACHE: [AtomicPtr<SlabCache>; MAX_CPUS] =
    [const { AtomicPtr::new(ptr::null_mut()) }; MAX_CPUS];

pub struct PerCpuCache;

pub struct PerCpuCacheGuard {
    flags: u64,
    ptr: *mut SlabCache,
}

impl PerCpuCache {
    fn cpu_slot() -> &'static AtomicPtr<SlabCache> {
        let id = X86_64::id();
        if id >= MAX_CPUS {
            panic!("CPU id {id} exceeds bootstrap slab-cache capacity");
        }

        &PER_CPU_SLAB_CACHE[id]
    }
}

impl SlabCacheStorage for PerCpuCache {
    fn store(ptr: *mut SlabCache) {
        Self::cpu_slot().store(ptr, Ordering::SeqCst);
    }

    fn get() -> impl DerefMut<Target = SlabCache> {
        let flags = X86_64::disable_interrupts();
        let ptr = Self::cpu_slot().load(Ordering::SeqCst);

        if ptr.is_null() {
            panic!("Attempted to use alloc/free before CPU initialisation!");
        }

        PerCpuCacheGuard { flags, ptr }
    }
}

impl Deref for PerCpuCacheGuard {
    type Target = SlabCache;

    fn deref(&self) -> &Self::Target {
        unsafe { &*self.ptr }
    }
}

impl DerefMut for PerCpuCacheGuard {
    fn deref_mut(&mut self) -> &mut Self::Target {
        unsafe { &mut *self.ptr }
    }
}

impl Drop for PerCpuCacheGuard {
    fn drop(&mut self) {
        X86_64::restore_interrupt_state(self.flags);
    }
}

pub type KernelHeap =
    KHeap<X86_64, PerCpuCache, PgAllocGetter, PageOffsetTranslator, StaticSlabGetter>;

#[global_allocator]
static K_HEAP: KernelHeap = KernelHeap::new();

#[allow(dead_code)]
pub fn init_for_this_cpu() {
    let page: ClaimedPage<X86_64, PgAllocGetter, PageOffsetTranslator> =
        ClaimedPage::alloc_zeroed().expect("Cannot allocate heap page");

    let slab_cache = unsafe { SlabCache::from_page(page) };
    PerCpuCache::store(slab_cache);
}
