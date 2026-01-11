use core::{mem, ops::Range};

use super::HeapRange;

/// Conservatively scan a word range for candidate GC pointers.
///
/// ## Why this exists
///
/// LLVM statepoints + stack maps give the runtime **precise** root locations
/// for managed frames. During bring-up/debugging, or when walking stacks that
/// contain non-managed/native frames, stackmap coverage can be incomplete.
///
/// When enabled, this conservative scan can be used as a fallback so the runtime
/// can continue operating. This may keep garbage alive due to **false positives**
/// (any in-heap, aligned word is treated as a potential pointer).
///
/// The callback receives a pointer to the *slot* containing the candidate value,
/// allowing a moving collector to update it.
pub fn conservative_scan_words(
  range: Range<*const usize>,
  heap: HeapRange,
  mut f: impl FnMut(*mut *mut u8),
) {
  let align = mem::align_of::<usize>();

  let mut slot = range.start;
  let end_addr = range.end as usize;

  while (slot as usize) < end_addr {
    // Safety: caller guarantees `range` is a valid readable range of `usize`s.
    let word = unsafe { slot.read() };

    // Candidate pointer heuristic:
    // - non-null
    // - aligned
    // - within heap bounds
    if word != 0 && (word % align == 0) {
      let candidate = word as *const u8;
      if heap.contains(candidate) && heap.passes_object_start_check(candidate) {
        // Safety: `slot` points to a machine word on the stack (or other scanned
        // memory). The caller/GC may treat it as a pointer slot and mutate it.
        let slot_ptr = slot as *mut *mut u8;
        f(slot_ptr);
      }
    }

    // Safety: same as above; slot advances within the provided range.
    slot = unsafe { slot.add(1) };
  }
}

