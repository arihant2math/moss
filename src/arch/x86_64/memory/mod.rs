use super::X86_64;
use libkernel::{
    memory::{
        address::VA,
        allocators::{
            phys::{FrameAllocator, PageAllocGetter},
            smalloc::{RegionList, Smalloc},
        },
        proc_vm::pg_offset::PageOffsetTranslator as LibPageOffsetTranslator,
        region::PhysMemoryRegion,
    },
    sync::{once_lock::OnceLock, spinlock::SpinLockIrq},
};

pub mod heap;

pub const PAGE_OFFSET: usize = 0xffff_8000_0000_0000;
pub const IMAGE_BASE: VA = VA::from_value(0xffff_ffff_8000_0000);

pub type PageOffsetTranslator = LibPageOffsetTranslator<{ PAGE_OFFSET }>;

const STATIC_REGION_COUNT: usize = 128;

static INIT_MEM_REGIONS: [PhysMemoryRegion; STATIC_REGION_COUNT] =
    [PhysMemoryRegion::empty(); STATIC_REGION_COUNT];
static INIT_RES_REGIONS: [PhysMemoryRegion; STATIC_REGION_COUNT] =
    [PhysMemoryRegion::empty(); STATIC_REGION_COUNT];

pub static INITIAL_ALLOCATOR: SpinLockIrq<Option<Smalloc<PageOffsetTranslator>>, X86_64> =
    SpinLockIrq::new(Some(Smalloc::new(
        RegionList::new(STATIC_REGION_COUNT, INIT_MEM_REGIONS.as_ptr().cast_mut()),
        RegionList::new(STATIC_REGION_COUNT, INIT_RES_REGIONS.as_ptr().cast_mut()),
    )));

pub static PAGE_ALLOC: OnceLock<FrameAllocator<X86_64>, X86_64> = OnceLock::new();

pub struct PgAllocGetter;

impl PageAllocGetter<X86_64> for PgAllocGetter {
    fn global_page_alloc() -> &'static FrameAllocator<X86_64> {
        PAGE_ALLOC.get().unwrap()
    }
}
