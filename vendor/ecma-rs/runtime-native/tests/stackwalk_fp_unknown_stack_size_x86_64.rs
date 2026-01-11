#![cfg(all(target_arch = "x86_64", target_os = "linux"))]

use runtime_native::arch::SafepointContext;
use runtime_native::stackmaps::{Location, StackMaps, StackSize, X86_64_DWARF_REG_RBP};
use runtime_native::stackwalk::StackBounds;
use runtime_native::statepoints::StatepointRecord;
use runtime_native::{walk_gc_root_pairs_from_fp, walk_gc_root_pairs_from_safepoint_context, walk_gc_roots_from_fp};

const FIXTURE: &[u8] = include_bytes!("fixtures/bin/statepoint_dynamic_alloca_x86_64.bin");

#[test]
fn stackwalk_supports_fp_relative_locations_with_unknown_stack_size() {
  let stackmaps = StackMaps::parse(FIXTURE).expect("parse + index");
  let (callsite_ra, callsite) = stackmaps.iter().next().expect("expected 1 callsite");
  assert_eq!(callsite.stack_size, StackSize::Unknown);

  let sp = StatepointRecord::new(callsite.record).expect("decode statepoint layout");
  assert_eq!(sp.gc_pair_count(), 1);
  let pair = &sp.gc_pairs()[0];
  assert_eq!(
    pair.base,
    Location::Indirect {
      size: 8,
      dwarf_reg: X86_64_DWARF_REG_RBP,
      offset: -16,
    },
    "fixture should use FP-relative spill slots when stack_size is unknown"
  );
  assert_eq!(pair.base, pair.derived, "fixture should have base==derived");

  // Synthetic stack memory.
  let mut stack = vec![0u8; 512];
  let base = stack.as_mut_ptr() as usize;
  let bounds = StackBounds::new(base as u64, (base + stack.len()) as u64).unwrap();

  // Two-frame chain: runtime frame -> managed caller -> 0.
  let start_fp = align_up(base + 0x100, 16);
  let caller_fp = align_up(base + 0x180, 16);
  let slot_addr = caller_fp.wrapping_sub(16);

  unsafe {
    // runtime -> caller
    write_u64(start_fp + 0, caller_fp as u64);
    write_u64(start_fp + 8, callsite_ra);
    // caller -> null
    write_u64(caller_fp + 0, 0);
    write_u64(caller_fp + 8, 0);
    // Seed the root slot value (not used by the pair walker, but required by the base walker).
    write_u64(slot_addr, 0xdead_beef);
  }

  // Walking from FP must work without consulting `stack_size`, because all locations are FP-based.
  let mut visited_base = Vec::<usize>::new();
  unsafe {
    walk_gc_roots_from_fp(start_fp as u64, Some(bounds), &stackmaps, |slot| {
      visited_base.push(slot as usize);
    })
    .expect("walk roots from fp");
  }
  visited_base.sort_unstable();
  visited_base.dedup();
  assert_eq!(visited_base, vec![slot_addr]);

  // Pair walker should report (slot, slot).
  let mut visited_pairs = Vec::<(usize, usize)>::new();
  unsafe {
    walk_gc_root_pairs_from_fp(start_fp as u64, Some(bounds), &stackmaps, |_ra, pairs| {
      for &(base_slot, derived_slot) in pairs {
        visited_pairs.push((base_slot as usize, derived_slot as usize));
      }
    })
    .expect("walk pairs from fp");
  }
  visited_pairs.sort_unstable();
  assert_eq!(visited_pairs, vec![(slot_addr, slot_addr)]);

  // Top-frame safepoint walking should also work even if no SP info is provided, because we don't
  // need SP to evaluate FP-relative locations.
  let ctx = SafepointContext {
    sp_entry: 0,
    sp: 0,
    fp: caller_fp,
    ip: callsite_ra as usize,
  };
  let mut visited_pairs_ctx = Vec::<(usize, usize)>::new();
  unsafe {
    walk_gc_root_pairs_from_safepoint_context(&ctx, Some(bounds), &stackmaps, |_ra, pairs| {
      for &(base_slot, derived_slot) in pairs {
        visited_pairs_ctx.push((base_slot as usize, derived_slot as usize));
      }
    })
    .expect("walk pairs from ctx");
  }
  visited_pairs_ctx.sort_unstable();
  assert_eq!(visited_pairs_ctx, vec![(slot_addr, slot_addr)]);
}

fn align_up(v: usize, align: usize) -> usize {
  (v + (align - 1)) & !(align - 1)
}

unsafe fn write_u64(addr: usize, val: u64) {
  (addr as *mut u64).write_unaligned(val);
}

