use runtime_native::{relocate_pair, StatepointRootPair};

#[test]
fn relocate_pair_base_eq_derived() {
  let mut base: usize = 0x1000;
  let mut derived: usize = 0x1000;

  unsafe {
    relocate_pair(
      StatepointRootPair {
        base_slot: (&mut base) as *mut usize,
        derived_slot: (&mut derived) as *mut usize,
      },
      |old| {
        assert_eq!(old, 0x1000);
        0x2000
      },
    );
  }

  assert_eq!(base, 0x2000);
  assert_eq!(derived, 0x2000);
}

#[test]
fn relocate_pair_interior_pointer_offset_preserved() {
  let mut base: usize = 0x1000;
  let mut derived: usize = 0x1000 + 0x30;

  unsafe {
    relocate_pair(
      StatepointRootPair {
        base_slot: (&mut base) as *mut usize,
        derived_slot: (&mut derived) as *mut usize,
      },
      |_old| 0x5000,
    );
  }

  assert_eq!(base, 0x5000);
  assert_eq!(derived, 0x5000 + 0x30);
}

#[test]
fn relocate_pair_null_pair_stays_null() {
  let mut base: usize = 0;
  let mut derived: usize = 0;

  unsafe {
    relocate_pair(
      StatepointRootPair {
        base_slot: (&mut base) as *mut usize,
        derived_slot: (&mut derived) as *mut usize,
      },
      |old| {
        assert_eq!(old, 0);
        0
      },
    );
  }

  assert_eq!(base, 0);
  assert_eq!(derived, 0);
}

#[test]
fn relocate_pair_null_derived_stays_null() {
  let mut base: usize = 0x1000;
  let mut derived: usize = 0;

  unsafe {
    relocate_pair(
      StatepointRootPair {
        base_slot: (&mut base) as *mut usize,
        derived_slot: (&mut derived) as *mut usize,
      },
      |_old| 0x9000,
    );
  }

  assert_eq!(base, 0x9000);
  assert_eq!(derived, 0);
}
