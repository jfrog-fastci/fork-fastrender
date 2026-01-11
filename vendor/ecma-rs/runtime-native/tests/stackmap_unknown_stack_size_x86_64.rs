#![cfg(all(target_arch = "x86_64", target_os = "linux"))]

use runtime_native::scan::{slot_addr_x86_64, ScanError};
use runtime_native::stackmaps::{Location, StackMap, StackMaps, StackSize, X86_64_DWARF_REG_RBP, X86_64_DWARF_REG_RSP};
use runtime_native::statepoints::StatepointRecord;
use stackmap_context::ThreadContext;

const FIXTURE: &[u8] = include_bytes!("fixtures/bin/statepoint_dynamic_alloca_x86_64.bin");

#[test]
fn parses_unknown_stack_size_dynamic_alloca_fixture() {
  let stackmap = StackMap::parse(FIXTURE).expect("parse .llvm_stackmaps blob");
  assert_eq!(stackmap.version, 3);
  assert_eq!(stackmap.functions.len(), 1);
  assert_eq!(stackmap.functions[0].stack_size, StackSize::Unknown);

  let stackmaps = StackMaps::parse(FIXTURE).expect("parse + build callsite index");
  let (_pc, callsite) = stackmaps.iter().next().expect("expected 1 callsite");
  assert_eq!(callsite.stack_size, StackSize::Unknown);

  let sp = StatepointRecord::new(callsite.record).expect("decode statepoint layout");
  assert_eq!(sp.gc_pair_count(), 1);
  let pair = &sp.gc_pairs()[0];

  // With dynamic allocas, LLVM 18 still emits usable root slots by switching to
  // FP-based addressing.
  assert_eq!(
    pair.base,
    Location::Indirect {
      size: 8,
      dwarf_reg: X86_64_DWARF_REG_RBP,
      offset: -16
    }
  );
  assert_eq!(pair.base, pair.derived);
}

#[test]
fn slot_addr_x86_64_supports_rbp_relative_locations_when_stack_size_is_unknown() {
  let stackmaps = StackMaps::parse(FIXTURE).expect("parse + index");
  let (_pc, callsite) = stackmaps.iter().next().expect("expected 1 callsite");
  let sp = StatepointRecord::new(callsite.record).expect("decode statepoint layout");
  let pair = &sp.gc_pairs()[0];
  let loc = &pair.base;

  // Synthetic stack memory; choose an FP value with enough headroom for the negative offset.
  let mut mem = vec![0u8; 128];
  let base = mem.as_mut_ptr() as u64;
  let fp = align_up(base + 64, 16);
  let expected_slot = add_i32(fp, loc_offset_i32(loc));

  let addr = slot_addr_x86_64(fp, StackSize::Unknown, false, None, loc).expect("slot addr");
  assert_eq!(addr as u64, expected_slot);
}

#[test]
fn slot_addr_x86_64_errors_for_rsp_relative_locations_when_stack_size_is_unknown() {
  let loc = Location::Indirect {
    size: 8,
    dwarf_reg: X86_64_DWARF_REG_RSP,
    offset: 0,
  };
  let err = slot_addr_x86_64(0x1000, StackSize::Unknown, false, None, &loc).unwrap_err();
  assert!(matches!(
    err,
    ScanError::UnknownStackSizeForRspBasedLocation { .. }
  ));
}

#[test]
fn slot_addr_x86_64_uses_saved_rsp_for_top_frame() {
  let regs = ThreadContext {
    rsp: 0x2000,
    ..Default::default()
  };
  let loc = Location::Indirect {
    size: 8,
    dwarf_reg: X86_64_DWARF_REG_RSP,
    offset: 8,
  };
  let addr = slot_addr_x86_64(0, StackSize::Unknown, true, Some(&regs), &loc).expect("slot addr");
  assert_eq!(addr as u64, 0x2008);
}

fn loc_offset_i32(loc: &Location) -> i32 {
  match *loc {
    Location::Indirect { offset, .. } => offset,
    _ => panic!("expected Indirect location, got {loc:?}"),
  }
}

fn align_up(v: u64, align: u64) -> u64 {
  debug_assert!(align.is_power_of_two());
  (v + (align - 1)) & !(align - 1)
}

fn add_i32(base: u64, offset: i32) -> u64 {
  if offset >= 0 {
    base + (offset as u64)
  } else {
    base - ((-offset) as u64)
  }
}
