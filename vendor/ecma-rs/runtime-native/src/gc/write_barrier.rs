use super::{ObjHeader, CARD_SIZE, YOUNG_SPACE};
use crate::mutator::current_mutator_thread_ptr;

/// Ensure `obj` is marked as remembered and, if it was newly remembered, enqueue
/// it on the current thread's buffer for merging at the next minor GC start.
///
/// # Safety
/// `obj` must point to the start of a valid GC-managed object.
#[inline]
pub(crate) unsafe fn remember_object(obj: *mut u8) -> bool {
  let header = &*(obj as *const ObjHeader);

  // When an object first becomes remembered, enqueue it on the current thread's
  // buffer; it will be merged into the global set at the next minor GC start.
  if header.set_remembered_if_unset() {
    let thread = current_mutator_thread_ptr();
    if !thread.is_null() {
      // `rt_write_barrier` is `NoGC` and must not allocate. Ensure the per-thread
      // buffer has spare capacity before pushing.
      let buf = &mut (*thread).new_remembered;
      if buf.len() == buf.capacity() {
        std::process::abort();
      }
      buf.push(obj);
    }
    return true;
  }
  false
}

/// Generational write barrier (internal implementation).
///
/// See `docs/write_barrier.md` for the ABI contract. This helper is used by the
/// exported `rt_write_barrier` entrypoint.
///
/// # Safety
/// - `obj` must point to the start of a GC-managed object.
/// - `slot` must be the address of a pointer-sized field inside `obj` (post-store).
#[inline]
pub unsafe fn write_barrier(obj: *mut u8, slot: *mut u8) -> bool {
  // Safety: caller must satisfy the write-barrier ABI contract.
  let value = (slot as *const *mut u8).read();

  if value.is_null() {
    return false;
  }

  // Only old→young pointers need to be remembered.
  if !YOUNG_SPACE.contains(value as usize) {
    return false;
  }

  // Writes into young objects don't need a barrier: nursery tracing will find
  // the edge.
  if YOUNG_SPACE.contains(obj as usize) {
    return false;
  }

  let newly_remembered = remember_object(obj);

  // Optional per-object card table for large pointer arrays.
  let header = &*(obj as *const ObjHeader);
  let card_table = header.card_table_ptr();
  if !card_table.is_null() {
    let slot_offset = (slot as usize).wrapping_sub(obj as usize);
    let card = slot_offset / CARD_SIZE;
    super::card_table::mark_card_range(card_table, card, card);
  }

  newly_remembered
}
