use super::format::{Location, StackMapRecord};

/// A `(base, derived)` relocation pair for a single GC pointer.
///
/// LLVM statepoints model derived pointers explicitly (e.g. interior pointers).
/// The runtime typically uses the base pointer to identify the owning object,
/// and updates the derived pointer after relocation.
#[derive(Debug, Clone, Copy)]
pub struct GcRootPair<'a> {
    pub base: &'a Location,
    pub derived: &'a Location,
}

/// A decoded view of a `gc.statepoint` StackMap record.
///
/// Layout (as emitted by LLVM 18 / StackMap v3 for statepoints):
/// - 3 header constants:
///   1. callconv
///   2. flags
///   3. num_deopt_args
/// - `num_deopt_args` locations for deoptimization state (ignored by our GC)
/// - remaining locations are `(base, derived)` relocation pairs
#[derive(Debug, Clone, Copy)]
pub struct StatepointRecordView<'a> {
    pub call_conv: u64,
    pub flags: u64,
    pub deopt_args: &'a [Location],
    gc_roots_flat: &'a [Location],
}

impl<'a> StatepointRecordView<'a> {
    pub fn decode(record: &'a StackMapRecord) -> Option<Self> {
        let locs = record.locations();
        if locs.len() < 3 {
            return None;
        }

        let call_conv = locs[0].as_u64()?;
        let flags = locs[1].as_u64()?;
        let num_deopt = usize::try_from(locs[2].as_u64()?).ok()?;

        let header_len = 3usize;
        let deopt_end = header_len.checked_add(num_deopt)?;
        let deopt_args = locs.get(header_len..deopt_end)?;
        let gc_roots_flat = locs.get(deopt_end..)?;

        if gc_roots_flat.len() % 2 != 0 {
            return None;
        }

        Some(Self {
            call_conv,
            flags,
            deopt_args,
            gc_roots_flat,
        })
    }

    pub fn num_gc_roots(&self) -> usize {
        self.gc_roots_flat.len() / 2
    }

    pub fn gc_root_pairs(&self) -> impl Iterator<Item = GcRootPair<'a>> + 'a {
        self.gc_roots_flat.chunks_exact(2).map(|pair| GcRootPair {
            base: &pair[0],
            derived: &pair[1],
        })
    }
}

