use std::mem;
use std::ptr;

use proptest::prelude::*;

use runtime_native::gc::card_table::{CardTable, CARD_SIZE};
use runtime_native::gc::{ObjHeader, SimpleRememberedSet, TypeDescriptor};
use runtime_native::test_util::TestGcGuard;
use runtime_native::{MutatorThread, ThreadContextGuard};

#[test]
fn card_table_marking_marks_exact_expected_card() {
  let table = CardTable::new(2048);
  table.mark_slot(600);
  assert_eq!(table.dirty_cards(), vec![600 / CARD_SIZE]);

  let table = CardTable::new(2048);
  table.mark_slot(0);
  assert_eq!(table.dirty_cards(), vec![0]);
}

#[test]
fn card_table_scan_and_rebuild_keeps_only_cards_that_still_need_scanning() {
  let mut table = CardTable::new(2048);
  // Mark cards 0, 1 and 3.
  table.mark_slot(0);
  table.mark_slot(CARD_SIZE + 8);
  table.mark_slot(CARD_SIZE * 3 + 1);

  let any = table.scan_and_rebuild(|start, _end| start == CARD_SIZE);
  assert!(any);
  assert_eq!(table.dirty_cards(), vec![1]);

  let any = table.scan_and_rebuild(|_, _| false);
  assert!(!any);
  assert!(table.dirty_cards().is_empty());
}

proptest! {
  #[test]
  fn proptest_card_table_mark_slot_marks_expected_card(object_size in 1usize..10_000, slot_offset in 0usize..10_000) {
    prop_assume!(slot_offset < object_size);

    let table = CardTable::new(object_size);
    table.mark_slot(slot_offset);

    prop_assert_eq!(table.dirty_cards(), vec![slot_offset / CARD_SIZE]);
  }
}

#[repr(C)]
struct PtrArrayObject<const N: usize> {
  header: ObjHeader,
  elems: [*mut u8; N],
}

#[test]
fn card_table_objects_can_be_evicted_from_remset_when_all_cards_clean() {
  let _gc = TestGcGuard::new();
  runtime_native::clear_write_barrier_state_for_tests();

  const N: usize = 128;

  let elem_offset = mem::offset_of!(PtrArrayObject<N>, elems);
  let ptr_size = mem::size_of::<*mut u8>();
  let mut offs = Vec::<u32>::with_capacity(N);
  for i in 0..N {
    offs.push((elem_offset + i * ptr_size) as u32);
  }
  let offs: &'static [u32] = Box::leak(offs.into_boxed_slice());
  let desc: &'static TypeDescriptor =
    Box::leak(Box::new(TypeDescriptor::new(mem::size_of::<PtrArrayObject<N>>(), offs)));

  let mut card_table = CardTable::new(mem::size_of::<PtrArrayObject<N>>());

  let mut old_obj = Box::new(PtrArrayObject::<N> {
    header: ObjHeader::new(desc),
    elems: [ptr::null_mut(); N],
  });
  unsafe {
    old_obj.header.set_card_table_ptr(card_table.as_ptr());
  }

  let young_value = Box::new(0u8);
  let old_value = Box::new(1u8);

  let young_start = (&*young_value as *const u8) as *mut u8;
  let young_end = unsafe { young_start.add(mem::size_of::<u8>()) };
  runtime_native::rt_gc_set_young_range(young_start, young_end);

  let mut thread = MutatorThread::new();
  let _guard = ThreadContextGuard::install(&mut thread);

  // Write a young pointer into the pointer array and invoke the barrier.
  //
  // Choose an index that crosses a 512B card boundary so we exercise non-zero cards.
  const INDEX: usize = 62;
  old_obj.elems[INDEX] = (&*young_value as *const u8) as *mut u8;
  let obj_ptr = (&mut *old_obj as *mut PtrArrayObject<N>) as *mut u8;
  let slot_ptr = (&mut old_obj.elems[INDEX] as *mut *mut u8) as *mut u8;

  unsafe { runtime_native::rt_write_barrier(obj_ptr, slot_ptr) };
  assert!(old_obj.header.is_remembered());
  assert_eq!(thread.new_remembered, vec![obj_ptr]);

  let expected_card = ((slot_ptr as usize) - (obj_ptr as usize)) / CARD_SIZE;
  assert_eq!(card_table.dirty_cards(), vec![expected_card]);

  // Merge newly-remembered objects into the global remset.
  let mut remset = SimpleRememberedSet::new();
  remset.prepare_for_minor_gc(std::iter::once(&mut thread));
  assert!(remset.contains(obj_ptr));

  // Remove the young pointer without calling the barrier (old→old store).
  old_obj.elems[INDEX] = (&*old_value as *const u8) as *mut u8;
  assert!(old_obj.header.is_remembered());

  let young_start = young_start as usize;
  let young_end = young_end as usize;

  remset.scan_and_rebuild(|obj| unsafe {
    card_table.scan_and_rebuild(|start, end| {
      let mut card_has_young = false;
      let base = obj as *const u8;
      for &offset in desc.ptr_offsets() {
        let off = offset as usize;
        if off >= start && off < end {
          let slot = base.add(off) as *const *mut u8;
          let value = ptr::read(slot);
          let addr = value as usize;
          if !value.is_null() && addr >= young_start && addr < young_end {
            card_has_young = true;
            break;
          }
        }
      }
      card_has_young
    })
  });

  // With no cards remaining dirty, the object is removed from the remembered set.
  assert!(!remset.contains(obj_ptr));
  assert!(!old_obj.header.is_remembered());
  assert!(card_table.dirty_cards().is_empty());
}
