use core::mem;
use core::sync::atomic::Ordering;

use crate::array;
use crate::trap;

use super::obj_size;
use super::for_each_ptr_slot;
use super::card_table_word_count;
use super::ObjHeader;
use super::TypeDescriptor;
use super::CARD_SIZE;

/// Iterate pointer slots in `obj`, scanning only dirty cards for pointer arrays.
///
/// If the object has no card table installed, this falls back to scanning all
/// pointer slots (equivalent to [`for_each_ptr_slot`]).
///
/// # Safety
/// - `obj` must point to the start of a valid GC-managed object.
/// - The object must be fully initialized, at least for all pointer slots.
pub(crate) unsafe fn for_each_ptr_slot_in_dirty_cards(mut obj: *mut u8, mut f: impl FnMut(*mut *mut u8)) {
  debug_assert!(!obj.is_null());

  // Follow forwarding pointers (nursery evacuation).
  let header = &*(obj as *const ObjHeader);
  if header.is_forwarded() {
    obj = header.forwarding_ptr();
  }

  let header = &*(obj as *const ObjHeader);
  let card_table = header.card_table_ptr();
  if card_table.is_null() {
    for_each_ptr_slot(obj, f);
    return;
  }

  let desc = header.type_desc();
  for &offset in desc.ptr_offsets() {
    let offset = offset as usize;
    debug_assert!(offset % mem::align_of::<*mut u8>() == 0);
    debug_assert!(offset + mem::size_of::<*mut u8>() <= desc.size);
    let slot = obj.add(offset) as *mut *mut u8;
    f(slot);
  }

  let size = obj_size(obj);
  if size == 0 {
    return;
  }

  let card_count = size.div_ceil(CARD_SIZE);
  let word_count = card_table_word_count(size);

  if header.type_desc == &array::RT_ARRAY_TYPE_DESC as *const TypeDescriptor {
    let arr = &*(obj as *const array::RtArrayHeader);
    if (arr.elem_flags & array::RT_ARRAY_FLAG_PTR_ELEMS) != 0 {
      if arr.elem_size as usize != mem::size_of::<*mut u8>() {
        trap::rt_trap_invalid_arg("pointer array elem_size must equal pointer size");
      }

      let payload_bytes = arr
        .len
        .checked_mul(mem::size_of::<*mut u8>())
        .unwrap_or_else(|| trap::rt_trap_invalid_arg("array size overflow"));
      let data_start = array::RT_ARRAY_DATA_OFFSET;
      let data_end = data_start
        .checked_add(payload_bytes)
        .unwrap_or_else(|| trap::rt_trap_invalid_arg("array size overflow"));

      let base = obj.add(array::RT_ARRAY_DATA_OFFSET) as *mut *mut u8;

      for word_idx in 0..word_count {
        let mut bits = (*card_table.add(word_idx)).swap(0, Ordering::AcqRel);
        #[cfg(feature = "gc_stats")]
        crate::gc_stats::record_cards_scanned_minor(bits.count_ones() as u64);
        while bits != 0 {
          let bit = bits.trailing_zeros() as usize;
          bits &= bits - 1;

          let card = word_idx * 64 + bit;
          if card >= card_count {
            continue;
          }

          let card_start = card * CARD_SIZE;
          let card_end = (card_start + CARD_SIZE).min(size);

          let start = card_start.max(data_start);
          let end = card_end.min(data_end);
          if start >= end {
            continue;
          }

          let elem_start = (start - data_start) / mem::size_of::<*mut u8>();
          let elem_end = (end - data_start) / mem::size_of::<*mut u8>();
          for i in elem_start..elem_end {
            let slot = base.add(i);
            f(slot);
          }
        }
      }

      return;
    }
  }

  // Object has a card table but is not a pointer array. Clear all bits so the
  // next minor collection doesn't rescan stale cards.
  for word_idx in 0..word_count {
    let _bits = (*card_table.add(word_idx)).swap(0, Ordering::AcqRel);
    #[cfg(feature = "gc_stats")]
    crate::gc_stats::record_cards_scanned_minor(_bits.count_ones() as u64);
  }
}
