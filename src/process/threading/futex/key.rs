use crate::{memory::page::ClaimedPage, sched::syscall_ctx::ProcessCtx};
use libkernel::error::{KernelError, Result};
use libkernel::memory::address::{TUA, VA};
use libkernel::memory::address_space::{AccessKind, UserAddressSpace};
use libkernel::memory::proc_vm::vmarea::VMAreaKind;

#[derive(PartialEq, Eq, PartialOrd, Ord, Hash, Clone, Copy, Debug)]
pub enum FutexKey {
    Private { pid: u32, addr: usize },
    Shared { frame: usize, offset: usize },
}

impl FutexKey {
    pub fn new_private(ctx: &ProcessCtx, uaddr: TUA<u32>) -> Self {
        let pid = ctx.shared().process.tgid.value();

        Self::Private {
            pid,
            addr: uaddr.value(),
        }
    }

    pub fn new_shared(ctx: &ProcessCtx, uaddr: TUA<u32>) -> Result<Self> {
        let proc_vm = ctx.shared().vm.shared_vm();
        let mut vm = proc_vm.lock_save_irq();
        let va = VA::from_value(uaddr.value());

        // A different process may have faulted in this shared page while our
        // PTE is still absent. Resolve its canonical frame from the backing.
        if let Some(vma) = vm.find_vma_for_fault(va, AccessKind::Read)
            && let VMAreaKind::SharedAnon(mapping) = vma.kind()
        {
            let offset = va.page_aligned().value() - vma.region.start_address().value();
            let frame = mapping.claim_page(offset)?;
            // SAFETY: claim_page transfers an owned reference, released on return.
            let _page = unsafe { ClaimedPage::from_pfn(frame) };
            return Ok(Self::Shared {
                frame: frame.value(),
                offset: uaddr.page_offset(),
            });
        }

        let pg_info = vm
            .mm_mut()
            .address_space_mut()
            .translate(va)
            .ok_or(KernelError::Fault)?;

        Ok(Self::Shared {
            frame: pg_info.pfn.value(),
            offset: uaddr.page_offset(),
        })
    }
}
