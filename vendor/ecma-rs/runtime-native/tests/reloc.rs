use runtime_native::reloc::relocate_derived_pair;

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

