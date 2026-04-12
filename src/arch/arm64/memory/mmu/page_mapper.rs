use libkernel::{
    memory::paging::{PageTableMapper, PgTable, PgTableArray},
    error::Result,
    memory::address::{TPA, TVA},
};

use crate::memory::PageOffsetTranslator;

pub struct PageOffsetPgTableMapper {}

impl PageTableMapper for PageOffsetPgTableMapper {
    unsafe fn with_page_table<T: PgTable, R>(
        &mut self,
        pa: TPA<PgTableArray<T>>,
        f: impl FnOnce(TVA<PgTableArray<T>>) -> R,
    ) -> Result<R> {
        Ok(f(pa.to_va::<PageOffsetTranslator>()))
    }
}
