#![cfg(all(target_arch = "x86_64", target_os = "linux"))]

use runtime_native::stackmaps::{Location, StackMap, StackMaps, StackSize, X86_64_DWARF_REG_RBP};
use runtime_native::statepoints::{eval_location, RegFile, RootSlot, StatepointRecord};

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
fn eval_location_supports_rbp_relative_locations_when_stack_size_is_unknown() {
  let stackmaps = StackMaps::parse(FIXTURE).expect("parse + index");
  let (_pc, callsite) = stackmaps.iter().next().expect("expected 1 callsite");
  let sp = StatepointRecord::new(callsite.record).expect("decode statepoint layout");
  let pair = &sp.gc_pairs()[0];
  let loc = &pair.base;

  struct FakeRegs {
    rbp: u64,
  }

  impl RegFile for FakeRegs {
    fn get(&self, dwarf_reg: u16) -> Option<u64> {
      match dwarf_reg {
        X86_64_DWARF_REG_RBP => Some(self.rbp),
        _ => None,
      }
    }
  }

  let mut mem = vec![0u8; 128];
  let rbp = align_up(mem.as_mut_ptr() as usize + 64, 16) as u64;
  let regs = FakeRegs { rbp };

  let RootSlot::StackAddr(addr) = eval_location(loc, &regs).expect("eval location") else {
    panic!("expected stack addr for Indirect location");
  };
  let Location::Indirect { offset, .. } = *loc else {
    panic!("expected Indirect location");
  };
  assert_eq!(addr as usize as u64, add_i32(rbp, offset));
}

fn align_up(v: usize, align: usize) -> usize {
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
