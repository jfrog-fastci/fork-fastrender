#![cfg(target_arch = "x86_64")]

use runtime_native::scan::scan_reloc_pairs;
use runtime_native::stackmaps::{Location, StackMaps};
use runtime_native::statepoints::StatepointRecord;
use stackmap_context::{ThreadContext, DWARF_REG_IP, DWARF_REG_SP};

const FIXTURE: &[u8] = include_bytes!("fixtures/bin/statepoint_base_derived_x86_64.bin");
const FIXTURE_DEOPT: &[u8] = include_bytes!("fixtures/bin/statepoint_deopt_x86_64.bin");

fn rewrite_first_patchpoint_id(bytes: &mut [u8], patchpoint_id: u64) {
  // StackMap v3 header:
  //   u8  Version
  //   u8  Reserved0
  //   u16 Reserved1
  //   u32 NumFunctions
  //   u32 NumConstants
  //   u32 NumRecords
  //
  // Followed by:
  //   StackSizeRecord[NumFunctions] (24 bytes each)
  //   u64 Constants[NumConstants]
  //   StackMapRecord[NumRecords] ...
  const HEADER_SIZE: usize = 16;
  const FUNCTION_RECORD_SIZE: usize = 24;
  const CONSTANT_SIZE: usize = 8;
  if bytes.len() < HEADER_SIZE {
    panic!("fixture too small to contain stackmap header");
  }
  if bytes[0] != 3 {
    panic!("fixture is not a StackMap v3 blob (version={})", bytes[0]);
  }
  let num_functions = u32::from_le_bytes(bytes[4..8].try_into().unwrap()) as usize;
  let num_constants = u32::from_le_bytes(bytes[8..12].try_into().unwrap()) as usize;
  let record0_off =
    HEADER_SIZE + num_functions * FUNCTION_RECORD_SIZE + num_constants * CONSTANT_SIZE;
  if record0_off + 8 > bytes.len() {
    panic!("fixture too small to contain first stackmap record header");
  }
  bytes[record0_off..record0_off + 8].copy_from_slice(&patchpoint_id.to_le_bytes());
}

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
  let pairs = scan_reloc_pairs(&ctx, &stackmaps).expect("scan");
  for (base_slot, derived_slot) in pairs {
    unsafe {
      seen.push((
        base_slot as usize,
        derived_slot as usize,
        base_slot.read_unaligned(),
        derived_slot.read_unaligned(),
      ));
    }
  }

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

#[test]
fn scan_reloc_pairs_accepts_custom_statepoint_id() {
  let mut bytes = FIXTURE.to_vec();
  rewrite_first_patchpoint_id(&mut bytes, 42);
  let stackmaps = StackMaps::parse(&bytes).expect("parse stackmaps fixture");
  let (callsite_ra, callsite) = stackmaps.iter().next().expect("fixture should contain callsites");
  assert_eq!(
    callsite.record.patchpoint_id, 42,
    "expected patchpoint_id override to take effect in parsed stackmap record"
  );

  // Synthetic stack memory (word-aligned).
  let mut stack: Vec<usize> = vec![0; 256];
  let sp_base = stack.as_mut_ptr() as u64;

  let mut ctx = ThreadContext::default();
  ctx.set_dwarf_reg_u64(DWARF_REG_IP, callsite_ra).unwrap();
  ctx.set_dwarf_reg_u64(DWARF_REG_SP, sp_base).unwrap();

  let pairs = scan_reloc_pairs(&ctx, &stackmaps).expect("scan");
  assert_eq!(pairs.len(), callsite.reloc_pairs().count());
}

#[test]
fn scan_reloc_pairs_skips_deopt_operands() {
  let stackmaps = StackMaps::parse(FIXTURE_DEOPT).expect("parse stackmaps fixture");
  let (callsite_ra, callsite) = stackmaps.iter().next().expect("fixture should contain callsites");

  let sp = StatepointRecord::new(callsite.record).expect("decode statepoint layout");
  assert_eq!(sp.header().callconv, 8);
  assert_eq!(sp.header().flags, 1);
  assert_eq!(sp.header().deopt_count, 2);
  assert_eq!(sp.deopt_locations().len(), 2);

  // Synthetic stack memory (word-aligned).
  let mut stack: Vec<usize> = vec![0; 256];
  let sp_base = stack.as_mut_ptr() as u64;

  let deopt0 = &sp.deopt_locations()[0];
  let deopt0_addr = slot_addr(sp_base, deopt0);

  let mut ctx = ThreadContext::default();
  ctx.set_dwarf_reg_u64(DWARF_REG_IP, callsite_ra).unwrap();
  ctx.set_dwarf_reg_u64(DWARF_REG_SP, sp_base).unwrap();

  let pairs = scan_reloc_pairs(&ctx, &stackmaps).expect("scan");

  assert_eq!(pairs.len(), sp.gc_pair_count());
  for (base_slot, derived_slot) in pairs {
    let base_addr = base_slot as usize;
    let derived_addr = derived_slot as usize;
    assert_ne!(
      base_addr, deopt0_addr,
      "deopt operand spill slot must not be reported as a relocation pair slot"
    );
    assert_ne!(
      derived_addr, deopt0_addr,
      "deopt operand spill slot must not be reported as a relocation pair slot"
    );
  }
}
