use std::alloc::{alloc_zeroed, dealloc, Layout};
use std::ptr::NonNull;
use std::sync::atomic::{AtomicU64, Ordering};

/// Card size in bytes.
///
/// This mirrors [`crate::gc::CARD_SIZE`], but is re-exported from this module
/// for convenience in tests.
pub const CARD_SIZE: usize = super::CARD_SIZE;

/// Mark a contiguous range of cards dirty in an object's per-object card table bitset.
///
/// `card_table` points to an array of [`AtomicU64`] words, where each bit corresponds to a card.
///
/// The pointer is expected to be stored in [`super::ObjHeader`] via
/// [`super::ObjHeader::set_card_table_ptr`].
///
/// # Safety
/// - `card_table` must be either null or point to at least `end_card / 64 + 1` valid `AtomicU64`
///   words.
pub unsafe fn mark_card_range(card_table: *mut AtomicU64, start_card: usize, end_card: usize) {
  if card_table.is_null() {
    return;
  }
  debug_assert!(start_card <= end_card);

  let start_word = start_card / 64;
  let end_word = end_card / 64;
  let start_bit = start_card % 64;
  let end_bit = end_card % 64;

  if start_word == end_word {
    let high_mask = if end_bit == 63 {
      !0u64
    } else {
      (1u64 << (end_bit + 1)) - 1
    };
    let low_mask = (!0u64) << start_bit;
    let mask = high_mask & low_mask;
    (*card_table.add(start_word)).fetch_or(mask, Ordering::Release);
    return;
  }

  // First word: mark from start_bit..=63.
  (*card_table.add(start_word)).fetch_or((!0u64) << start_bit, Ordering::Release);

  // Middle words: mark all bits.
  for word in (start_word + 1)..end_word {
    (*card_table.add(word)).fetch_or(!0u64, Ordering::Release);
  }

  // Last word: mark 0..=end_bit.
  let last_mask = if end_bit == 63 {
    !0u64
  } else {
    (1u64 << (end_bit + 1)) - 1
  };
  (*card_table.add(end_word)).fetch_or(last_mask, Ordering::Release);
}

/// Per-object card table for large pointer-array objects.
///
/// The table is *sticky* across minor GCs: dirty cards remain dirty until a
/// minor GC scans them and determines they no longer contain any pointers into
/// the current young generation.
#[derive(Debug)]
pub struct CardTable {
  object_size: usize,
  card_count: usize,
  ptr: NonNull<AtomicU64>,
  layout: Layout,
  word_count: usize,
}

// SAFETY: Card tables are immutable in size and only mutate their internal atomics. They can be
// moved between threads as long as the caller provides any required GC synchronization.
unsafe impl Send for CardTable {}
unsafe impl Sync for CardTable {}

impl CardTable {
  pub fn new(object_size: usize) -> Self {
    let card_count = object_size.div_ceil(CARD_SIZE).max(1);
    let word_count = card_count.div_ceil(64);

    let bytes = word_count * core::mem::size_of::<AtomicU64>();
    // `ObjHeader::set_card_table_ptr` stores the pointer in the high bits of `meta` and requires
    // the low meta-flag bits be free. The current flag mask uses the low 4 bits, so allocate the
    // bitset aligned to 16 bytes (mask + 1).
    let layout =
      Layout::from_size_align(bytes, super::META_FLAGS_MASK + 1).expect("invalid card table layout");
    let ptr = unsafe { alloc_zeroed(layout) }.cast::<AtomicU64>();
    let ptr = NonNull::new(ptr).unwrap_or_else(|| std::alloc::handle_alloc_error(layout));

    Self {
      object_size,
      card_count,
      ptr,
      layout,
      word_count,
    }
  }

  pub fn as_ptr(&self) -> *mut AtomicU64 {
    self.ptr.as_ptr()
  }

  pub fn card_count(&self) -> usize {
    self.card_count
  }

  /// Mark the card containing `slot_offset` as dirty.
  ///
  /// `slot_offset` is the byte offset of a pointer slot relative to the object base.
  #[inline]
  pub fn mark_slot(&self, slot_offset: usize) {
    let card = slot_offset / CARD_SIZE;
    debug_assert!(
      card < self.card_count,
      "slot_offset ({slot_offset}) out of bounds for object_size ({})",
      self.object_size
    );
    if card >= self.card_count {
      return;
    }
    unsafe {
      mark_card_range(self.as_ptr(), card, card);
    }
  }

  pub fn is_card_dirty(&self, card_index: usize) -> bool {
    if card_index >= self.card_count {
      return false;
    }
    let word = card_index / 64;
    let bit = card_index % 64;
    debug_assert!(word < self.word_count);
    let w = unsafe { (*self.as_ptr().add(word)).load(Ordering::Relaxed) };
    (w & (1u64 << bit)) != 0
  }

  pub fn dirty_cards(&self) -> Vec<usize> {
    let mut out = Vec::new();
    for word_idx in 0..self.word_count {
      let mut word = unsafe { (*self.as_ptr().add(word_idx)).load(Ordering::Relaxed) };
      while word != 0 {
        let bit = word.trailing_zeros() as usize;
        let card = word_idx * 64 + bit;
        if card >= self.card_count {
          break;
        }
        out.push(card);
        word &= word - 1;
      }
    }
    out
  }

  /// Scan dirty cards and rebuild the dirty set based on `scan_card`.
  ///
  /// For each dirty card, calls `scan_card(start, end)` where `start`/`end` are byte offsets
  /// relative to the object base. If `scan_card` returns `true`, the card remains dirty; otherwise
  /// the card is cleared.
  ///
  /// Returns `true` if any cards remain dirty after rebuilding.
  pub fn scan_and_rebuild(&mut self, mut scan_card: impl FnMut(usize, usize) -> bool) -> bool {
    let mut any_dirty = false;

    for word_idx in 0..self.word_count {
      let mut word = unsafe { (*self.as_ptr().add(word_idx)).load(Ordering::Relaxed) };
      if word == 0 {
        continue;
      }

      while word != 0 {
        let bit = word.trailing_zeros() as usize;
        let card_idx = word_idx * 64 + bit;
        if card_idx >= self.card_count {
          break;
        }

        let start = card_idx * CARD_SIZE;
        let end = (start + CARD_SIZE).min(self.object_size);

        if scan_card(start, end) {
          any_dirty = true;
        } else {
          // Clear the bit. (No concurrent marking during STW GC.)
          let mask = !(1u64 << bit);
          unsafe {
            (*self.as_ptr().add(word_idx)).fetch_and(mask, Ordering::Relaxed);
          }
        }

        word &= word - 1;
      }
    }

    any_dirty
  }
}

impl Drop for CardTable {
  fn drop(&mut self) {
    unsafe { dealloc(self.ptr.as_ptr().cast::<u8>(), self.layout) }
  }
}

