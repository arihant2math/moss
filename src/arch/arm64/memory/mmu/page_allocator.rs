use core::marker::PhantomData;

use crate::memory::page::ClaimedPage;
use libkernel::{
    memory::paging::{PageAllocator, PgTable, PgTableArray},
    error::Result,
    memory::address::TPA,
};

pub struct PageTableAllocator<'a> {
    data: PhantomData<&'a u8>,
}

impl PageTableAllocator<'_> {
    pub fn new() -> Self {
        Self { data: PhantomData }
    }
}

impl PageAllocator for PageTableAllocator<'_> {
    fn allocate_page_table<T: PgTable>(&mut self) -> Result<TPA<PgTableArray<T>>> {
        let pg = ClaimedPage::alloc_zeroed()?;

        Ok(pg.leak().pa().cast())
    }
}
