use runtime_native::gc_roots::{relocate_reloc_pairs_in_place, RelocPair};
use runtime_native::statepoints::RootSlot;
use stackmap_context::ThreadContext;

#[test]
fn reloc_pairs_handles_repeated_base_slots() {
  // Fake "stack frame" containing GC-related slots.
  let mut frame: [usize; 4] = [0; 4];

  let base: usize = 0x1000;
  frame[0] = base;
  frame[1] = base + 0x10;
  frame[2] = base + 0x20;
  frame[3] = 0xDEAD_BEEF;

  let base_slot = RootSlot::StackAddr((&mut frame[0] as *mut usize).cast::<u8>());
  let derived1_slot = RootSlot::StackAddr((&mut frame[1] as *mut usize).cast::<u8>());
  let derived2_slot = RootSlot::StackAddr((&mut frame[2] as *mut usize).cast::<u8>());

  // Ordering intentionally puts the base slot's own relocate first, then derived pointers that
  // reference the same base. A naive in-place algorithm would relocate the base slot multiple times
  // and/or compute derived offsets from the already-updated base.
  let pairs = [
    RelocPair {
      base_slot,
      derived_slot: base_slot,
    },
    RelocPair {
      base_slot,
      derived_slot: derived1_slot,
    },
    RelocPair {
      base_slot,
      derived_slot: derived2_slot,
    },
    // Duplicate derived slot to ensure idempotence when the stackmap contains repeated locations.
    RelocPair {
      base_slot,
      derived_slot: derived1_slot,
    },
  ];

  let mut calls = 0usize;
  let mut ctx = ThreadContext::default();
  relocate_reloc_pairs_in_place(&mut ctx, pairs, |old| {
    calls += 1;
    old + 0x1000
  });

  // Only one unique base slot is relocated.
  assert_eq!(calls, 1);
  assert_eq!(frame[0], 0x2000);
  assert_eq!(frame[1], 0x2010);
  assert_eq!(frame[2], 0x2020);
  assert_eq!(frame[3], 0xDEAD_BEEF);
}

#[test]
fn reloc_pairs_treats_zero_as_null() {
  let mut frame: [usize; 2] = [0; 2];

  let base_slot = &mut frame[0] as *mut usize;
  let derived_slot = &mut frame[1] as *mut usize;

  let pairs = [RelocPair {
    base_slot: RootSlot::StackAddr(base_slot.cast::<u8>()),
    derived_slot: RootSlot::StackAddr(derived_slot.cast::<u8>()),
  }];

  let mut calls = 0usize;
  let mut ctx = ThreadContext::default();
  relocate_reloc_pairs_in_place(&mut ctx, pairs, |old| {
    calls += 1;
    old + 0x1000
  });

  assert_eq!(calls, 0);
  assert_eq!(frame, [0, 0]);
}
