#![cfg(target_arch = "x86_64")]

use runtime_native::scan::scan_reloc_pairs;
use runtime_native::stackmaps::{Location, StackMaps};
use runtime_native::statepoints::StatepointRecord;
use runtime_native::statepoints::X86_64_DWARF_REG_FP;
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

fn slot_addr(sp_base: u64, fp_base: u64, loc: &Location) -> usize {
  match *loc {
    Location::Indirect {
      dwarf_reg,
      offset,
      size: _,
    } => {
      let base = if dwarf_reg == DWARF_REG_SP {
        sp_base
      } else if dwarf_reg == X86_64_DWARF_REG_FP {
        fp_base
      } else {
        panic!("unexpected dwarf_reg={dwarf_reg} (expected SP={DWARF_REG_SP} or FP={X86_64_DWARF_REG_FP})");
      };
      add_signed(base, offset) as usize
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
  let mut stack: Vec<usize> = vec![0; 512];
  let base = stack.as_mut_ptr() as u64;
  // Use a base pointer in the middle of the scratch space so both positive (SP-style) and negative
  // (FP-style) offsets remain in-bounds.
  let reg_base = base + (256 * std::mem::size_of::<usize>()) as u64;
  let sp_base = reg_base;
  let fp_base = reg_base;

  let same_base_addr = slot_addr(sp_base, fp_base, &same_pair.base);
  let same_derived_addr = slot_addr(sp_base, fp_base, &same_pair.derived);
  assert_eq!(
    same_base_addr, same_derived_addr,
    "base==derived pair must point to the same spill slot"
  );

  let derived_base_addr = slot_addr(sp_base, fp_base, &derived_pair.base);
  let derived_derived_addr = slot_addr(sp_base, fp_base, &derived_pair.derived);
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
  ctx.set_dwarf_reg_u64(X86_64_DWARF_REG_FP, fp_base).unwrap();

  let mut seen: Vec<(usize, usize, usize, usize)> = Vec::new();
  let pairs = scan_reloc_pairs(&mut ctx, &stackmaps).expect("scan");
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
  let stackmaps_orig = StackMaps::parse(FIXTURE).expect("parse original stackmaps fixture");
  let (_orig_callsite_ra, orig_callsite) = stackmaps_orig
    .iter()
    .next()
    .expect("fixture should contain callsites");
  let orig_pair_count = orig_callsite.reloc_pairs().count();

  let mut bytes = FIXTURE.to_vec();
  rewrite_first_patchpoint_id(&mut bytes, 42);
  let stackmaps = StackMaps::parse(&bytes).expect("parse stackmaps fixture");
  let (callsite_ra, callsite) = stackmaps.iter().next().expect("fixture should contain callsites");
  assert_eq!(
    callsite.record.patchpoint_id, 42,
    "expected patchpoint_id override to take effect in parsed stackmap record"
  );
  let reloc_pairs: Vec<_> = callsite.reloc_pairs().collect();
  assert_eq!(
    reloc_pairs.len(),
    orig_pair_count,
    "relocation pair count should be independent of patchpoint_id"
  );

  // Synthetic stack memory (word-aligned).
  let mut stack: Vec<usize> = vec![0; 512];
  let base = stack.as_mut_ptr() as u64;
  let reg_base = base + (256 * std::mem::size_of::<usize>()) as u64;
  let sp_base = reg_base;
  let fp_base = reg_base;

  // Seed the spill slots with a base pointer and a derived pointer (base + 16).
  let base_ptr: usize = 0x1111_2222_3333_4444;
  let delta: usize = 16;

  // Find a base!=derived pair so we can compute the derived slot address.
  let derived_pair = reloc_pairs
    .iter()
    .find(|p| p.base != p.derived)
    .expect("missing base!=derived pair");
  let base_addr = slot_addr(sp_base, fp_base, &derived_pair.base);
  let derived_addr = slot_addr(sp_base, fp_base, &derived_pair.derived);
  unsafe {
    (base_addr as *mut usize).write_unaligned(base_ptr);
    (derived_addr as *mut usize).write_unaligned(base_ptr + delta);
  }
  let mut ctx = ThreadContext::default();
  ctx.set_dwarf_reg_u64(DWARF_REG_IP, callsite_ra).unwrap();
  ctx.set_dwarf_reg_u64(DWARF_REG_SP, sp_base).unwrap();
  ctx.set_dwarf_reg_u64(X86_64_DWARF_REG_FP, fp_base).unwrap();

  let pairs = scan_reloc_pairs(&mut ctx, &stackmaps).expect("scan");
  assert!(
    pairs.iter().any(|&(b, d)| b as usize == base_addr && d as usize == derived_addr),
    "expected scan to return the derived pair even with custom patchpoint_id"
  );
  assert_eq!(pairs.len(), orig_pair_count);
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
  let mut stack: Vec<usize> = vec![0; 512];
  let base = stack.as_mut_ptr() as u64;
  let reg_base = base + (256 * std::mem::size_of::<usize>()) as u64;
  let sp_base = reg_base;
  let fp_base = reg_base;

  let deopt0 = &sp.deopt_locations()[0];
  let deopt0_addr = slot_addr(sp_base, fp_base, deopt0);

  let mut ctx = ThreadContext::default();
  ctx.set_dwarf_reg_u64(DWARF_REG_IP, callsite_ra).unwrap();
  ctx.set_dwarf_reg_u64(DWARF_REG_SP, sp_base).unwrap();
  ctx.set_dwarf_reg_u64(X86_64_DWARF_REG_FP, fp_base).unwrap();

  let pairs = scan_reloc_pairs(&mut ctx, &stackmaps).expect("scan");

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

#[test]
fn invalid_statepoint_layout_yields_no_reloc_pairs_or_slots() {
  // Build a record that starts with the statepoint-style 3-constant header, but has an invalid
  // `deopt_count` so decoding as a `gc.statepoint` fails. Both `CallSite::reloc_pairs` and
  // `scan_reloc_pairs` should treat it as a non-statepoint record and return nothing.
  let mut bytes: Vec<u8> = Vec::new();

  fn push_u8(out: &mut Vec<u8>, v: u8) {
    out.push(v);
  }
  fn push_u16(out: &mut Vec<u8>, v: u16) {
    out.extend_from_slice(&v.to_le_bytes());
  }
  fn push_u32(out: &mut Vec<u8>, v: u32) {
    out.extend_from_slice(&v.to_le_bytes());
  }
  fn push_u64(out: &mut Vec<u8>, v: u64) {
    out.extend_from_slice(&v.to_le_bytes());
  }
  fn push_i32(out: &mut Vec<u8>, v: i32) {
    out.extend_from_slice(&v.to_le_bytes());
  }
  fn align_to_8(out: &mut Vec<u8>) {
    while out.len() % 8 != 0 {
      out.push(0);
    }
  }
  fn push_loc(out: &mut Vec<u8>, kind: u8, size: u16, dwarf_reg: u16, offset_or_const: i32) {
    push_u8(out, kind);
    push_u8(out, 0); // reserved0
    push_u16(out, size);
    push_u16(out, dwarf_reg);
    push_u16(out, 0); // reserved1
    push_i32(out, offset_or_const);
  }

  // Header.
  push_u8(&mut bytes, 3); // version
  push_u8(&mut bytes, 0); // reserved0
  push_u16(&mut bytes, 0); // reserved1
  push_u32(&mut bytes, 1); // num_functions
  push_u32(&mut bytes, 0); // num_constants
  push_u32(&mut bytes, 1); // num_records

  // Function record.
  push_u64(&mut bytes, 0); // address
  push_u64(&mut bytes, 32); // stack_size
  push_u64(&mut bytes, 1); // record_count

  // Record header.
  push_u64(&mut bytes, 0x1234); // patchpoint_id (arbitrary)
  push_u32(&mut bytes, 0x1234); // instruction_offset => callsite pc = 0x1234
  push_u16(&mut bytes, 0); // reserved
  push_u16(&mut bytes, 5); // num_locations = 3 header consts + 2 extra locs

  // 3 constant header locations (callconv, flags, deopt_count=100 (invalid)).
  push_loc(&mut bytes, 4, 8, 0, 0); // callconv
  push_loc(&mut bytes, 4, 8, 0, 0); // flags
  push_loc(&mut bytes, 4, 8, 0, 100); // deopt_count (invalid: exceeds locations)

  // Two Indirect locations (these are not real relocation pairs; they exist so the record looks
  // plausible under non-statepoint scanning/validation paths).
  push_loc(&mut bytes, 3, 8, DWARF_REG_SP, 0);
  push_loc(&mut bytes, 3, 8, DWARF_REG_SP, 8);

  // Live-out header.
  align_to_8(&mut bytes);
  push_u16(&mut bytes, 0); // padding
  push_u16(&mut bytes, 0); // num_live_outs
  align_to_8(&mut bytes);

  let stackmaps = StackMaps::parse(&bytes).expect("parse StackMaps");
  let (callsite_ra, callsite) = stackmaps.iter().next().expect("callsite");
  assert_eq!(callsite.record.patchpoint_id, 0x1234);
  assert_eq!(callsite_ra, 0x1234);

  assert_eq!(callsite.reloc_pairs().count(), 0);

  let mut ctx = ThreadContext::default();
  ctx.set_dwarf_reg_u64(DWARF_REG_IP, callsite_ra).unwrap();
  ctx.set_dwarf_reg_u64(DWARF_REG_SP, 0).unwrap();
  let pairs = scan_reloc_pairs(&mut ctx, &stackmaps).expect("scan");
  assert!(pairs.is_empty());
}
