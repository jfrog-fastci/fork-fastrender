use std::alloc::{alloc_zeroed, dealloc, handle_alloc_error, Layout};
use std::collections::{HashMap, HashSet};
use std::mem::{align_of, size_of};
use std::ptr::NonNull;
use std::sync::atomic::{AtomicU64, Ordering};

use proptest::prelude::*;

use runtime_native::gc::{ObjHeader, CARD_SIZE, OBJ_HEADER_SIZE};
use runtime_native::test_util::TestGcGuard;

const WORD_BYTES: usize = size_of::<usize>();
const YOUNG_OBJ_WORDS: usize = 1;

fn align_up(offset: usize, align: usize) -> usize {
  debug_assert!(align.is_power_of_two());
  (offset + (align - 1)) & !(align - 1)
}

#[derive(Clone, Copy, Debug)]
struct YoungRange {
  start: usize,
  end: usize,
}

impl YoungRange {
  fn new(start: *mut u8, end: *mut u8) -> Self {
    Self {
      start: start as usize,
      end: end as usize,
    }
  }

  fn start_ptr(self) -> *mut u8 {
    self.start as *mut u8
  }

  fn end_ptr(self) -> *mut u8 {
    self.end as *mut u8
  }

  fn contains(self, ptr: *mut u8) -> bool {
    let p = ptr as usize;
    p >= self.start && p < self.end
  }
}

/// A trivially-copying semispace nursery model.
///
/// Young objects are allocated into from-space and copied to to-space during a
/// minor GC. After the copy, we flip semispaces (so to-space becomes the new
/// from-space).
struct Nursery {
  mem: Box<[usize]>,
  half_words: usize,
  from_first: bool,
  from_alloc_words: usize,
  young_objects: Vec<*mut u8>,
}

impl Nursery {
  fn new(half_words: usize) -> Self {
    let mem = vec![0usize; half_words * 2].into_boxed_slice();
    Self {
      mem,
      half_words,
      from_first: true,
      from_alloc_words: 0,
      young_objects: Vec::new(),
    }
  }

  fn base_ptr(&self) -> *mut u8 {
    self.mem.as_ptr() as *mut u8
  }

  fn half_bytes(&self) -> usize {
    self.half_words * WORD_BYTES
  }

  fn from_start(&self) -> *mut u8 {
    let base = self.base_ptr();
    let offset_words = if self.from_first { 0 } else { self.half_words };
    unsafe { base.add(offset_words * WORD_BYTES) }
  }

  fn to_start(&self) -> *mut u8 {
    let base = self.base_ptr();
    let offset_words = if self.from_first { self.half_words } else { 0 };
    unsafe { base.add(offset_words * WORD_BYTES) }
  }

  fn from_range(&self) -> YoungRange {
    let start = self.from_start();
    let end = unsafe { start.add(self.half_bytes()) };
    YoungRange::new(start, end)
  }

  fn to_range(&self) -> YoungRange {
    let start = self.to_start();
    let end = unsafe { start.add(self.half_bytes()) };
    YoungRange::new(start, end)
  }

  fn alloc_young_object(&mut self) -> *mut u8 {
    let need = self.from_alloc_words + YOUNG_OBJ_WORDS;
    assert!(
      need <= self.half_words,
      "nursery exhausted: need {need} words, have {}",
      self.half_words
    );

    let ptr = unsafe { self.from_start().add(self.from_alloc_words * WORD_BYTES) };
    self.from_alloc_words = need;
    self.young_objects.push(ptr);
    ptr
  }

  fn young_objects_set(&self) -> HashSet<usize> {
    self.young_objects.iter().map(|p| *p as usize).collect()
  }

  /// Copy `survivors` to to-space, then flip semispaces.
  ///
  /// Returns a forwarding map from old address → new address.
  fn minor_copy(&mut self, survivors: &HashSet<usize>) -> HashMap<usize, *mut u8> {
    let to_start = self.to_start() as usize;
    let mut to_alloc_words = 0usize;

    let mut forwarding = HashMap::<usize, *mut u8>::new();
    let mut new_young_objects = Vec::new();

    for &old_ptr in &self.young_objects {
      let old_ptr_usize = old_ptr as usize;
      if !survivors.contains(&old_ptr_usize) {
        continue;
      }

      let new_ptr = (to_start + to_alloc_words * WORD_BYTES) as *mut u8;
      to_alloc_words += YOUNG_OBJ_WORDS;
      assert!(
        to_alloc_words <= self.half_words,
        "to-space overflow: copied {to_alloc_words} words into to-space of {} words",
        self.half_words
      );

      forwarding.insert(old_ptr_usize, new_ptr);
      new_young_objects.push(new_ptr);
    }

    // Flip semispaces: to-space becomes new from-space.
    self.from_first = !self.from_first;
    self.from_alloc_words = to_alloc_words;
    self.young_objects = new_young_objects;

    forwarding
  }
}

struct CardTableAlloc {
  ptr: NonNull<AtomicU64>,
  words: usize,
  layout: Layout,
}

impl CardTableAlloc {
  fn new(object_bytes: usize) -> Self {
    let card_count = object_bytes.div_ceil(CARD_SIZE).max(1);
    let words = card_count.div_ceil(64).max(1);
    let size = words * size_of::<AtomicU64>();

    // `ObjHeader` stores the card table pointer in the high bits of `meta`, so
    // we need enough alignment to keep the low flag bits free (currently 4 bits
    // → 16-byte alignment).
    let layout = Layout::from_size_align(size, 16).expect("card table layout");
    let raw = unsafe { alloc_zeroed(layout) };
    if raw.is_null() {
      handle_alloc_error(layout);
    }
    let ptr = raw as *mut AtomicU64;
    for i in 0..words {
      unsafe {
        ptr.add(i).write(AtomicU64::new(0));
      }
    }

    Self {
      ptr: unsafe { NonNull::new_unchecked(ptr) },
      words,
      layout,
    }
  }

  fn as_ptr(&self) -> *mut AtomicU64 {
    self.ptr.as_ptr()
  }

  fn words(&self) -> usize {
    self.words
  }
}

impl Drop for CardTableAlloc {
  fn drop(&mut self) {
    unsafe {
      dealloc(self.ptr.as_ptr().cast::<u8>(), self.layout);
    }
  }
}

fn card_word_and_mask(card_idx: usize) -> (usize, u64) {
  let word = card_idx / 64;
  let bit = card_idx % 64;
  (word, 1u64 << bit)
}

unsafe fn card_is_marked(card_table: *mut AtomicU64, card_idx: usize) -> bool {
  let (word, mask) = card_word_and_mask(card_idx);
  let bits = (*card_table.add(word)).load(Ordering::Acquire);
  (bits & mask) != 0
}

unsafe fn card_clear(card_table: *mut AtomicU64, card_idx: usize) {
  let (word, mask) = card_word_and_mask(card_idx);
  (*card_table.add(word)).fetch_and(!mask, Ordering::AcqRel);
}

fn card_any_marked(card_table: *mut AtomicU64, words: usize) -> bool {
  for i in 0..words {
    let bits = unsafe { (*card_table.add(i)).load(Ordering::Acquire) };
    if bits != 0 {
      return true;
    }
  }
  false
}

struct OldObject {
  storage: Box<[usize]>,
  slots_offset: usize,
  num_ptr_slots: usize,
  object_bytes: usize,
  // Owned so the in-header raw pointer stays valid for the object's lifetime.
  #[allow(dead_code)]
  card_table: Option<CardTableAlloc>,
}

impl OldObject {
  fn alloc(num_ptr_slots: usize, has_card_table: bool) -> Self {
    let slots_offset = align_up(OBJ_HEADER_SIZE, align_of::<*mut u8>());
    let object_bytes = slots_offset + num_ptr_slots * size_of::<*mut u8>();
    let words = object_bytes.div_ceil(WORD_BYTES);

    let mut storage = vec![0usize; words].into_boxed_slice();
    let obj_ptr = storage.as_mut_ptr() as *mut u8;

    // The object memory is zero-initialized. That's sufficient for the model:
    // - `ObjHeader`'s `type_desc` is unused (we never call `rt_write_barrier_range`).
    // - `meta` starts at 0 (no forwarded/mark/remembered/pinned bits).
    let header = unsafe { &mut *(obj_ptr as *mut ObjHeader) };
    let card_table = has_card_table.then(|| CardTableAlloc::new(object_bytes));
    if let Some(ct) = card_table.as_ref() {
      unsafe {
        header.set_card_table_ptr(ct.as_ptr());
      }
    }

    Self {
      storage,
      slots_offset,
      num_ptr_slots,
      object_bytes,
      card_table,
    }
  }

  fn obj_ptr(&self) -> *mut u8 {
    self.storage.as_ptr() as *mut u8
  }

  fn header(&self) -> &ObjHeader {
    unsafe { &*(self.obj_ptr() as *const ObjHeader) }
  }

  fn slots_ptr(&self) -> *mut *mut u8 {
    unsafe { self.obj_ptr().add(self.slots_offset) as *mut *mut u8 }
  }

  fn slots(&self) -> &[*mut u8] {
    unsafe { std::slice::from_raw_parts(self.slots_ptr(), self.num_ptr_slots) }
  }

  fn slots_mut(&mut self) -> &mut [*mut u8] {
    unsafe { std::slice::from_raw_parts_mut(self.slots_ptr(), self.num_ptr_slots) }
  }

  fn card_count(&self) -> usize {
    self.object_bytes.div_ceil(CARD_SIZE).max(1)
  }

  fn card_table_ptr(&self) -> *mut AtomicU64 {
    self.header().card_table_ptr()
  }

  fn card_table_words(&self) -> usize {
    self.card_table.as_ref().map(|ct| ct.words()).unwrap_or(0)
  }

  fn card_contains_young(&self, card_idx: usize, young: YoungRange) -> bool {
    let obj_addr = self.obj_ptr() as usize;
    let start = card_idx * CARD_SIZE;
    let end = start + CARD_SIZE;

    for (slot_idx, &value) in self.slots().iter().enumerate() {
      let field_addr = unsafe { self.slots_ptr().add(slot_idx) as usize };
      let offset = field_addr
        .checked_sub(obj_addr)
        .expect("slot address must be within object allocation");
      if offset < start || offset >= end {
        continue;
      }
      if !value.is_null() && young.contains(value) {
        return true;
      }
    }

    false
  }
}

struct ModelHeap {
  nursery: Nursery,
  old_objects: Vec<OldObject>,
  old_ptrs: Vec<*mut u8>,
}

impl ModelHeap {
  fn new() -> Self {
    Self {
      nursery: Nursery::new(512),
      old_objects: Vec::new(),
      old_ptrs: Vec::new(),
    }
  }

  fn alloc_old_object(&mut self, num_ptr_slots: usize, has_card_table: bool) -> usize {
    let obj = OldObject::alloc(num_ptr_slots, has_card_table);
    let ptr = obj.obj_ptr();
    let idx = self.old_objects.len();
    self.old_objects.push(obj);
    self.old_ptrs.push(ptr);
    idx
  }

  fn alloc_young_object(&mut self) -> *mut u8 {
    self.nursery.alloc_young_object()
  }

  fn store_ptr(&mut self, old_idx: usize, slot_index: usize, value: *mut u8) {
    let old = &mut self.old_objects[old_idx];
    let slot_index = slot_index % old.num_ptr_slots;
    old.slots_mut()[slot_index] = value;

    let field_ptr = unsafe { old.slots_ptr().add(slot_index) as *mut u8 };
    unsafe {
      runtime_native::rt_write_barrier(old.obj_ptr(), field_ptr);
    }
  }

  fn referenced_young_set(&self) -> HashSet<usize> {
    let live_young = self.nursery.young_objects_set();
    let mut survivors = HashSet::new();
    for old in &self.old_objects {
      for &value in old.slots() {
        if value.is_null() {
          continue;
        }
        let ptr = value as usize;
        if live_young.contains(&ptr) {
          survivors.insert(ptr);
        }
      }
    }
    survivors
  }

  fn minor_gc(&mut self, survivors: &HashSet<usize>) {
    let old_young = self.nursery.from_range();
    let forward_map = self.nursery.minor_copy(survivors);
    let new_young = self.nursery.from_range();

    // Update the runtime's notion of the young range post-flip.
    runtime_native::rt_gc_set_young_range(new_young.start_ptr(), new_young.end_ptr());

    let mut keep = HashSet::<usize>::new();

    for old in &mut self.old_objects {
      let obj = old.obj_ptr();
      if !runtime_native::remembered_set_contains(obj) {
        continue;
      }

      let card_table = old.card_table_ptr();
      if !card_table.is_null() {
        let card_count = old.card_count();
        for card_idx in 0..card_count {
          let marked = unsafe { card_is_marked(card_table, card_idx) };
          if !marked {
            continue;
          }

          let mut card_has_young = false;
          let obj_usize = obj as usize;
          let slots_ptr = old.slots_ptr();
          let slots = old.slots_mut();

          for (slot_idx, slot) in slots.iter_mut().enumerate() {
            let field_ptr = unsafe { slots_ptr.add(slot_idx) as *mut u8 };
            let offset = (field_ptr as usize)
              .checked_sub(obj_usize)
              .expect("field must be within object");
            if offset / CARD_SIZE != card_idx {
              continue;
            }

            let value = *slot;
            if !value.is_null() && old_young.contains(value) {
              if let Some(&new_ptr) = forward_map.get(&(value as usize)) {
                *slot = new_ptr;
              }
            }

            if !(*slot).is_null() && new_young.contains(*slot) {
              card_has_young = true;
            }
          }

          if !card_has_young {
            unsafe { card_clear(card_table, card_idx) };
          }
        }

        if card_any_marked(card_table, old.card_table_words()) {
          keep.insert(obj as usize);
        }
      } else {
        for slot in old.slots_mut() {
          let value = *slot;
          if value.is_null() || !old_young.contains(value) {
            continue;
          }
          if let Some(&new_ptr) = forward_map.get(&(value as usize)) {
            *slot = new_ptr;
          }
        }

        if old
          .slots()
          .iter()
          .any(|&value| !value.is_null() && new_young.contains(value))
        {
          keep.insert(obj as usize);
        }
      }
    }

    runtime_native::remembered_set_scan_and_rebuild_for_tests(&self.old_ptrs, |obj| {
      keep.contains(&(obj as usize))
    });
  }

  fn assert_invariants(&self) {
    let young = self.nursery.from_range();
    let tospace = self.nursery.to_range();

    for old in &self.old_objects {
      let obj = old.obj_ptr();
      let in_remset = runtime_native::remembered_set_contains(obj);
      assert_eq!(
        in_remset,
        old.header().is_remembered(),
        "remembered-set membership must match ObjHeader::REMEMBERED bit"
      );

      let has_young_ptr = old
        .slots()
        .iter()
        .any(|&value| !value.is_null() && young.contains(value));

      if !in_remset {
        assert!(
          !has_young_ptr,
          "soundness: old object not in remset still contains a young pointer"
        );
      }

      if has_young_ptr {
        assert!(
          in_remset,
          "completeness: old object contains young pointer but is not in remset"
        );
        assert!(
          old.header().is_remembered(),
          "completeness: old object contains young pointer but REMEMBERED bit is not set"
        );
      }

      // After a semispace flip, all live young objects are in from-space. Any
      // pointer into to-space is necessarily stale (un-forwarded).
      for &value in old.slots() {
        if value.is_null() {
          continue;
        }
        assert!(
          !tospace.contains(value),
          "stale pointer: old object contains pointer into evacuated nursery space"
        );
      }

      let card_table = old.card_table_ptr();
      if !card_table.is_null() {
        for card_idx in 0..old.card_count() {
          let contains_young = old.card_contains_young(card_idx, young);
          let marked = unsafe { card_is_marked(card_table, card_idx) };
          if marked {
            assert!(
              contains_young,
              "card precision: marked card does not contain a young pointer"
            );
          }
          if contains_young {
            assert!(
              marked,
              "card precision: card contains a young pointer but is not marked"
            );
          }
        }
      }
    }

    // After rebuild, the remset should be precise (no stale entries).
    for old in &self.old_objects {
      let obj = old.obj_ptr();
      if !runtime_native::remembered_set_contains(obj) {
        continue;
      }
      let has_young_ptr = old
        .slots()
        .iter()
        .any(|&value| !value.is_null() && young.contains(value));
      assert!(
        has_young_ptr,
        "remset contains an old object that has no young pointers"
      );
    }
  }
}

#[test]
fn remembered_set_and_card_table_survive_multiple_minors() {
  let _gc = TestGcGuard::new();
  runtime_native::clear_write_barrier_state_for_tests();

  let mut heap = ModelHeap::new();
  let young = heap.nursery.from_range();
  runtime_native::rt_gc_set_young_range(young.start_ptr(), young.end_ptr());

  let plain = heap.alloc_old_object(4, false);
  // Ensure we span multiple cards with 512B cards: 160 slots ~= 1280B.
  let carded = heap.alloc_old_object(160, true);

  let a = heap.alloc_young_object();
  let b = heap.alloc_young_object();
  let c = heap.alloc_young_object();

  heap.store_ptr(plain, 0, a);
  heap.store_ptr(plain, 1, b);
  heap.store_ptr(carded, 0, a);
  heap.store_ptr(carded, 120, c);

  // GC #1: everything survives.
  let survivors = heap.referenced_young_set();
  heap.minor_gc(&survivors);
  heap.assert_invariants();

  // Mutate: remove some old→young pointers but leave others, without triggering
  // any writes for the remaining pointers (this catches "clear-all-after-minor").
  heap.store_ptr(plain, 1, std::ptr::null_mut()); // drop `b`
  heap.store_ptr(carded, 0, std::ptr::null_mut()); // drop `a` from one card

  // GC #2: `b` dies; `a` survives via `plain`; `c` survives via `carded`.
  let survivors = heap.referenced_young_set();
  heap.minor_gc(&survivors);
  heap.assert_invariants();

  // GC #3: no intervening writes; remembered objects/cards must remain tracked.
  let survivors = heap.referenced_young_set();
  heap.minor_gc(&survivors);
  heap.assert_invariants();

  // Clear remaining pointers and ensure the remset/card marks are cleared on rebuild.
  heap.store_ptr(plain, 0, std::ptr::null_mut());
  heap.store_ptr(carded, 120, std::ptr::null_mut());

  let survivors = heap.referenced_young_set();
  heap.minor_gc(&survivors);
  heap.assert_invariants();

  assert!(!runtime_native::remembered_set_contains(heap.old_objects[plain].obj_ptr()));
  assert!(!runtime_native::remembered_set_contains(heap.old_objects[carded].obj_ptr()));
  assert!(!heap.old_objects[plain].header().is_remembered());
  assert!(!heap.old_objects[carded].header().is_remembered());

  // Reset process-global write-barrier configuration (young range + remset tracking).
  runtime_native::clear_write_barrier_state_for_tests();
}

#[derive(Clone, Debug)]
enum Op {
  AllocOld { slots: u16, card: bool },
  AllocYoung,
  Store { old: u8, slot: u16, value: ValueRef },
  MinorGc,
}

#[derive(Clone, Debug)]
enum ValueRef {
  Null,
  Old(u8),
  Young(u8),
}

fn op_strategy() -> impl Strategy<Value = Vec<Op>> {
  let alloc_old =
    (1u16..=256u16, any::<bool>()).prop_map(|(slots, card)| Op::AllocOld { slots, card });
  let alloc_young = Just(Op::AllocYoung);
  let store = (any::<u8>(), any::<u16>(), any::<u8>()).prop_map(|(old, slot, tag)| {
    let value = match tag % 3 {
      0 => ValueRef::Null,
      1 => ValueRef::Old(tag),
      _ => ValueRef::Young(tag),
    };
    Op::Store { old, slot, value }
  });
  let gc = Just(Op::MinorGc);

  prop::collection::vec(prop_oneof![alloc_old, alloc_young, store, gc], 1..50)
}

proptest! {
  #![proptest_config(ProptestConfig {
    cases: 64,
    .. ProptestConfig::default()
  })]

  #[test]
  fn proptest_remembered_set_model(ops in op_strategy()) {
    let _gc = TestGcGuard::new();
    runtime_native::clear_write_barrier_state_for_tests();

    let mut heap = ModelHeap::new();
    let young = heap.nursery.from_range();
    runtime_native::rt_gc_set_young_range(young.start_ptr(), young.end_ptr());

    const MAX_OLD: usize = 20;

    for op in ops {
      match op {
        Op::AllocOld { slots, card } => {
          if heap.old_objects.len() >= MAX_OLD {
            continue;
          }
          heap.alloc_old_object(slots as usize, card);
        }
        Op::AllocYoung => {
          if heap.nursery.young_objects.len() >= 200 {
            continue;
          }
          heap.alloc_young_object();
        }
        Op::Store { old, slot, value } => {
          if heap.old_objects.is_empty() {
            continue;
          }
          let old_idx = (old as usize) % heap.old_objects.len();
          let value_ptr = match value {
            ValueRef::Null => std::ptr::null_mut(),
            ValueRef::Old(i) => {
              heap.old_objects[(i as usize) % heap.old_objects.len()].obj_ptr()
            }
            ValueRef::Young(i) => {
              if heap.nursery.young_objects.is_empty() {
                std::ptr::null_mut()
              } else {
                heap.nursery.young_objects[(i as usize) % heap.nursery.young_objects.len()]
              }
            }
          };
          heap.store_ptr(old_idx, slot as usize, value_ptr);
        }
        Op::MinorGc => {
          let survivors = heap.referenced_young_set();
          heap.minor_gc(&survivors);
          heap.assert_invariants();
        }
      }
    }

    // Always do two consecutive minors at the end to stress sticky rebuild behavior.
    let survivors = heap.referenced_young_set();
    heap.minor_gc(&survivors);
    heap.assert_invariants();
    let survivors = heap.referenced_young_set();
    heap.minor_gc(&survivors);
    heap.assert_invariants();

    // Reset process-global write-barrier configuration (young range + remset tracking).
    runtime_native::clear_write_barrier_state_for_tests();
  }
}
