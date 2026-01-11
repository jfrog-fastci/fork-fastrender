#![cfg(target_arch = "x86_64")]

use runtime_native::scan::scan_reloc_pairs;
use runtime_native::stackmaps::{Location, StackMaps};
use stackmap_context::{ThreadContext, DWARF_REG_IP, DWARF_REG_SP};

const FIXTURE: &[u8] = include_bytes!("fixtures/bin/statepoint_base_derived_x86_64.bin");

fn add_signed(base: u64, offset: i32) -> u64 {
  if offset >= 0 {
    base + (offset as u64)
  } else {
    base - ((-offset) as u64)
  }
}

fn slot_addr(sp_base: u64, loc: &Location) -> usize {
  match *loc {
    Location::Indirect {
      dwarf_reg,
      offset,
      size: _,
    } => {
      assert_eq!(dwarf_reg, DWARF_REG_SP, "fixture should use SP-relative Indirect slots");
      add_signed(sp_base, offset) as usize
    }
    _ => panic!("expected Indirect location, got {loc:?}"),
  }
}

#[test]
fn scan_reloc_pairs_reports_base_and_derived_spill_slots() {
  let stackmaps = StackMaps::parse(FIXTURE).expect("parse stackmaps fixture");
  let (callsite_ra, callsite) = stackmaps.iter().next().expect("fixture should contain callsites");

  let reloc_pairs: Vec<_> = callsite.reloc_pairs().collect();
  assert_eq!(
    reloc_pairs.len(),
    2,
    "fixture should contain exactly two (base, derived) pairs (one base==derived, one base!=derived)"
  );

  // Identify which pair is which; don't rely on LLVM's ordering.
  let (same_pair, derived_pair) = {
    let mut same = None;
    let mut derived = None;
    for pair in &reloc_pairs {
      if pair.base == pair.derived {
        same = Some(pair);
      } else {
        derived = Some(pair);
      }
    }
    (same.expect("missing base==derived pair"), derived.expect("missing base!=derived pair"))
  };

  // Synthetic stack memory (word-aligned).
  let mut stack: Vec<usize> = vec![0; 256];
  let sp_base = stack.as_mut_ptr() as u64;

  let same_base_addr = slot_addr(sp_base, &same_pair.base);
  let same_derived_addr = slot_addr(sp_base, &same_pair.derived);
  assert_eq!(
    same_base_addr, same_derived_addr,
    "base==derived pair must point to the same spill slot"
  );

  let derived_base_addr = slot_addr(sp_base, &derived_pair.base);
  let derived_derived_addr = slot_addr(sp_base, &derived_pair.derived);
  assert_ne!(
    derived_base_addr, derived_derived_addr,
    "base!=derived pair must use distinct spill slots"
  );

  // Seed the spill slots with a base pointer and a derived pointer (base + 16).
  let base_ptr: usize = 0x1111_2222_3333_4444;
  let delta: usize = 16;

  unsafe {
    (same_base_addr as *mut usize).write_unaligned(base_ptr);
    (derived_base_addr as *mut usize).write_unaligned(base_ptr);
    (derived_derived_addr as *mut usize).write_unaligned(base_ptr + delta);
  }

  let mut ctx = ThreadContext::default();
  ctx.set_dwarf_reg_u64(DWARF_REG_IP, callsite_ra).unwrap();
  ctx.set_dwarf_reg_u64(DWARF_REG_SP, sp_base).unwrap();

  let mut seen: Vec<(usize, usize, usize, usize)> = Vec::new();
  scan_reloc_pairs(&ctx, &stackmaps, |base_slot, derived_slot| unsafe {
    seen.push((
      base_slot as usize,
      derived_slot as usize,
      base_slot.read_unaligned(),
      derived_slot.read_unaligned(),
    ));
  })
  .expect("scan");

  assert_eq!(seen.len(), 2, "expected two relocation pairs from scan");

  // Validate that we can observe:
  // - one pair where base_slot==derived_slot and both read as base_ptr
  // - one pair where base_slot!=derived_slot and derived reads as base_ptr+delta
  let mut saw_same = false;
  let mut saw_derived = false;
  for (base_addr, derived_addr, base_val, derived_val) in seen {
    if base_addr == derived_addr {
      saw_same = true;
      assert_eq!(base_addr, same_base_addr);
      assert_eq!(base_val, base_ptr);
      assert_eq!(derived_val, base_ptr);
    } else {
      saw_derived = true;
      assert_eq!(base_addr, derived_base_addr);
      assert_eq!(derived_addr, derived_derived_addr);
      assert_eq!(base_val, base_ptr);
      assert_eq!(derived_val, base_ptr + delta);
    }
  }

  assert!(saw_same, "did not observe base==derived relocation pair");
  assert!(saw_derived, "did not observe base!=derived relocation pair");
}

