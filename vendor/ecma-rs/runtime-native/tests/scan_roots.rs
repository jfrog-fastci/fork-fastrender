#![cfg(target_arch = "x86_64")]

use runtime_native::scan::{scan_roots, RootVisitor};
use runtime_native::stackmaps::{Location, StackMaps};
use runtime_native::statepoints::X86_64_DWARF_REG_FP;
use stackmap_context::{ThreadContext, DWARF_REG_IP, DWARF_REG_SP};

const FIXTURE: &[u8] = include_bytes!("fixtures/bin/statepoint_base_derived_x86_64.bin");

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

#[derive(Default)]
struct Seen {
  roots: Vec<usize>,
  derived_pairs: Vec<(usize, usize)>,
}

impl RootVisitor for Seen {
  fn visit_root(&mut self, slot: *mut usize) {
    self.roots.push(slot as usize);
  }

  fn visit_derived_pair(&mut self, base_slot: *mut usize, derived_slot: *mut usize) {
    self
      .derived_pairs
      .push((base_slot as usize, derived_slot as usize));
  }
}

#[test]
fn scan_roots_splits_plain_roots_and_derived_pairs() {
  let stackmaps = StackMaps::parse(FIXTURE).expect("parse stackmaps fixture");
  let (callsite_ra, callsite) = stackmaps.iter().next().expect("fixture should contain callsites");

  let reloc_pairs: Vec<_> = callsite.reloc_pairs().collect();
  assert_eq!(reloc_pairs.len(), 2);

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

  let same_addr = slot_addr(sp_base, fp_base, &same_pair.base);
  let derived_base_addr = slot_addr(sp_base, fp_base, &derived_pair.base);
  let derived_derived_addr = slot_addr(sp_base, fp_base, &derived_pair.derived);

  // Seed the spill slots with a base pointer and a derived pointer (base + 16).
  let base_ptr: usize = 0x1111_2222_3333_4444;
  let delta: usize = 16;

  unsafe {
    (same_addr as *mut usize).write_unaligned(base_ptr);
    (derived_base_addr as *mut usize).write_unaligned(base_ptr);
    (derived_derived_addr as *mut usize).write_unaligned(base_ptr + delta);
  }

  let mut ctx = ThreadContext::default();
  ctx.set_dwarf_reg_u64(DWARF_REG_IP, callsite_ra).unwrap();
  ctx.set_dwarf_reg_u64(DWARF_REG_SP, sp_base).unwrap();
  ctx.set_dwarf_reg_u64(X86_64_DWARF_REG_FP, fp_base).unwrap();

  let mut seen = Seen::default();
  scan_roots(&mut ctx, &stackmaps, &mut seen).expect("scan_roots");

  seen.roots.sort_unstable();
  seen.derived_pairs.sort_unstable();

  assert_eq!(seen.roots, vec![same_addr]);
  assert_eq!(seen.derived_pairs, vec![(derived_base_addr, derived_derived_addr)]);
}
