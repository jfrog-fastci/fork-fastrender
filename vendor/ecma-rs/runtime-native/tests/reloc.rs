use runtime_native::reloc::{relocate_derived_pair, relocate_derived_pairs};

#[test]
fn relocates_derived_pointer_with_delta() {
  let mut base = 0x1000usize;
  let mut derived = 0x1008usize;

  relocate_derived_pair(&mut base as *mut usize, &mut derived as *mut usize, |base| base + 0x1000);

  assert_eq!(base, 0x2000);
  assert_eq!(derived, 0x2008);
}

#[test]
fn relocates_when_base_equals_derived() {
  let mut base = 0x1000usize;
  let mut derived = 0x1000usize;

  relocate_derived_pair(&mut base as *mut usize, &mut derived as *mut usize, |base| base + 0x1000);

  assert_eq!(base, 0x2000);
  assert_eq!(derived, 0x2000);
}

#[test]
fn null_base_zeros_both_slots() {
  let mut base = 0usize;
  let mut derived = 0x1234usize;

  relocate_derived_pair(&mut base as *mut usize, &mut derived as *mut usize, |base| base + 0x1000);

  assert_eq!(base, 0);
  assert_eq!(derived, 0);
}

#[test]
fn works_when_base_and_derived_share_a_slot() {
  let mut slot = 0x1000usize;

  relocate_derived_pair(&mut slot as *mut usize, &mut slot as *mut usize, |base| base + 0x1000);

  assert_eq!(slot, 0x2000);
}

#[test]
fn works_with_unaligned_slots() {
  // Stackmap slot addresses are derived from register + offset arithmetic; be robust to the slot
  // pointer not being naturally aligned for `usize`.
  let mut buf = [0u8; 2 * core::mem::size_of::<usize>() + 1];
  let base_slot = unsafe { buf.as_mut_ptr().add(1) as *mut usize };
  let derived_slot = unsafe { buf.as_mut_ptr().add(1 + core::mem::size_of::<usize>()) as *mut usize };

  unsafe {
    base_slot.write_unaligned(0x1000usize);
    derived_slot.write_unaligned(0x1008usize);
  }

  relocate_derived_pair(base_slot, derived_slot, |base| base + 0x1000);

  let base = unsafe { base_slot.read_unaligned() };
  let derived = unsafe { derived_slot.read_unaligned() };
  assert_eq!(base, 0x2000);
  assert_eq!(derived, 0x2008);
}

#[test]
fn relocate_derived_pairs_handles_shared_base_slot() {
  let mut base = 0x1000usize;
  let mut derived1 = 0x1008usize;
  let mut derived2 = 0x1010usize;

  let pairs = [
    (&mut base as *mut usize, &mut derived1 as *mut usize),
    (&mut base as *mut usize, &mut derived2 as *mut usize),
  ];

  relocate_derived_pairs(&pairs, |base| base + 0x1000);

  assert_eq!(base, 0x2000);
  assert_eq!(derived1, 0x2008);
  assert_eq!(derived2, 0x2010);
}

#[test]
fn relocate_derived_pairs_is_order_independent_with_shared_base_slot() {
  let mut base = 0x1000usize;
  let mut derived1 = 0x1008usize;
  let mut derived2 = 0x1010usize;

  // Reverse ordering to ensure we don't depend on processing order.
  let pairs = [
    (&mut base as *mut usize, &mut derived2 as *mut usize),
    (&mut base as *mut usize, &mut derived1 as *mut usize),
  ];

  relocate_derived_pairs(&pairs, |base| base + 0x1000);

  assert_eq!(base, 0x2000);
  assert_eq!(derived1, 0x2008);
  assert_eq!(derived2, 0x2010);
}

#[test]
fn relocate_derived_pairs_handles_base_reloc_pair_and_shared_base() {
  let mut base = 0x1000usize;
  let mut derived1 = 0x1008usize;
  let mut derived2 = 0x1010usize;

  // Include the `base == derived` self-pair and two derived pointers that share the base slot.
  let pairs = [
    (&mut base as *mut usize, &mut derived1 as *mut usize),
    (&mut base as *mut usize, &mut base as *mut usize),
    (&mut base as *mut usize, &mut derived2 as *mut usize),
  ];

  relocate_derived_pairs(&pairs, |base| base + 0x1000);

  assert_eq!(base, 0x2000);
  assert_eq!(derived1, 0x2008);
  assert_eq!(derived2, 0x2010);
}
