//! TLB invalidation helpers.

use crate::memory::paging::TLBInvalidator;

/// A no-op TLB invalidator used when invalidation is unnecessary.
pub struct NullTlbInvalidator {}

impl TLBInvalidator for NullTlbInvalidator {}
