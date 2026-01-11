use runtime_native::{
  walk_gc_root_pairs_from_fp, walk_gc_root_pairs_from_safepoint_context, walk_gc_roots_from_fp, StackMaps,
  WalkError,
};
use runtime_native::arch::SafepointContext;
use runtime_native::stackwalk::StackBounds;

#[test]
fn unknown_stack_size_is_not_required_for_pair_walking() {
  let bytes = build_stackmaps_with_unknown_stack_size();
  let stackmaps = StackMaps::parse(&bytes).expect("parse stackmaps");
  let (callsite_ra, _callsite) = stackmaps.iter().next().expect("callsite");

  let mut stack = vec![0u8; 512];
  let base = stack.as_mut_ptr() as usize;
  let bounds = StackBounds::new(base as u64, (base + stack.len()) as u64).unwrap();

  let caller_fp = align_up(base + 256, 16);
  let start_fp = align_up(base + 128, 16);

  // With forced frame pointers, the caller callsite SP is derived from the callee FP:
  //   caller_sp_callsite = callee_fp + 16
  let caller_sp_callsite = start_fp + 16;

  // Model one statepoint `(base, derived)` pair, where `derived` is an interior pointer derived from
  // `base` at a constant offset.
  let base_val = Box::into_raw(Box::new([0u8; 16])) as *mut u8 as u64;
  let derived_val = base_val + 8;

  unsafe {
    // runtime frame -> managed caller
    write_u64(start_fp + 0, caller_fp as u64);
    write_u64(start_fp + 8, callsite_ra);

    // managed caller -> null
    write_u64(caller_fp + 0, 0);
    write_u64(caller_fp + 8, 0);

    // base = [SP + 0], derived = [SP + 8]
    write_u64(caller_sp_callsite + 0, base_val);
    write_u64(caller_sp_callsite + 8, derived_val);
  }

  let expected = vec![(caller_sp_callsite + 0, caller_sp_callsite + 8)];

  // 1) `walk_gc_root_pairs_from_fp` must work even when the stackmap `stack_size` is unknown.
  let mut visited = Vec::<(usize, usize)>::new();
  unsafe {
    walk_gc_root_pairs_from_fp(start_fp as u64, Some(bounds), &stackmaps, |_ra, pairs| {
      for &(base_slot, derived_slot) in pairs {
        visited.push((base_slot as usize, derived_slot as usize));
      }
    })
    .expect("walk from fp");
  }
  visited.sort_unstable();
  assert_eq!(visited, expected);

  // 1b) `walk_gc_roots_from_fp` must also work: it derives callsite SP from the callee FP and does
  // not consult `stack_size` (even if the stackmap function record reports it as unknown).
  let mut visited_base = Vec::<usize>::new();
  unsafe {
    walk_gc_roots_from_fp(start_fp as u64, Some(bounds), &stackmaps, |slot| {
      visited_base.push(slot as usize);
    })
    .expect("walk roots from fp");
  }
  visited_base.sort_unstable();
  visited_base.dedup();
  assert_eq!(visited_base, vec![caller_sp_callsite + 0]);

  // 2) `walk_gc_root_pairs_from_safepoint_context` must work when a stackmap-semantics SP is
  // provided in the context, even if `stack_size` is unknown.
  let (sp_entry, sp) = sp_entry_and_sp(caller_sp_callsite);
  let ctx = SafepointContext {
    sp_entry,
    sp,
    fp: caller_fp,
    ip: callsite_ra as usize,
    regs: core::ptr::null_mut(),
  };
  let mut visited_ctx = Vec::<(usize, usize)>::new();
  unsafe {
    walk_gc_root_pairs_from_safepoint_context(&ctx, Some(bounds), &stackmaps, |_ra, pairs| {
      for &(base_slot, derived_slot) in pairs {
        visited_ctx.push((base_slot as usize, derived_slot as usize));
      }
    })
    .expect("walk from ctx with sp");
  }
  visited_ctx.sort_unstable();
  assert_eq!(visited_ctx, expected);

  // 2b) Same for the base-slot walker.
  let mut visited_base_ctx = Vec::<usize>::new();
  unsafe {
    runtime_native::stackwalk_fp::walk_gc_roots_from_safepoint_context(&ctx, Some(bounds), &stackmaps, |slot| {
      visited_base_ctx.push(slot as usize);
    })
    .expect("walk roots from ctx with sp");
  }
  visited_base_ctx.sort_unstable();
  visited_base_ctx.dedup();
  assert_eq!(visited_base_ctx, vec![caller_sp_callsite + 0]);

  // 2c) If `ctx.sp == 0` but `ctx.sp_entry` is available, the walker can still reconstruct the
  // stackmap-semantics SP without consulting `stack_size`:
  // - x86_64: `sp = sp_entry + 8` (call pushes return address)
  // - aarch64: `sp == sp_entry`
  let ctx_entry_only = SafepointContext {
    sp_entry,
    sp: 0,
    fp: caller_fp,
    ip: callsite_ra as usize,
    regs: core::ptr::null_mut(),
  };
  let mut visited_entry_only = Vec::<(usize, usize)>::new();
  unsafe {
    walk_gc_root_pairs_from_safepoint_context(&ctx_entry_only, Some(bounds), &stackmaps, |_ra, pairs| {
      for &(base_slot, derived_slot) in pairs {
        visited_entry_only.push((base_slot as usize, derived_slot as usize));
      }
    })
    .expect("walk from ctx with sp_entry only");
  }
  visited_entry_only.sort_unstable();
  assert_eq!(visited_entry_only, expected);

  let mut visited_base_entry_only = Vec::<usize>::new();
  unsafe {
    runtime_native::stackwalk_fp::walk_gc_roots_from_safepoint_context(
      &ctx_entry_only,
      Some(bounds),
      &stackmaps,
      |slot| visited_base_entry_only.push(slot as usize),
    )
    .expect("walk roots from ctx with sp_entry only");
  }
  visited_base_entry_only.sort_unstable();
  visited_base_entry_only.dedup();
  assert_eq!(visited_base_entry_only, vec![caller_sp_callsite + 0]);

  // 3) If neither `ctx.sp` nor `ctx.sp_entry` is available, the walker must not try to reconstruct
  // stackmap-semantics `SP` from the stackmap function record's `stack_size` (it is a fixed frame
  // size and can be wrong when the callsite performs outgoing-arg stack adjustments).
  //
  // Instead, it surfaces an explicit error.
  let ctx_missing_sp = SafepointContext {
    sp_entry: 0,
    sp: 0,
    fp: caller_fp,
    ip: callsite_ra as usize,
    regs: core::ptr::null_mut(),
  };
  let res = unsafe {
    walk_gc_root_pairs_from_safepoint_context(&ctx_missing_sp, Some(bounds), &stackmaps, |_ra, _pairs| {})
  };
  assert!(matches!(
    res,
    Err(WalkError::MissingStackmapSp { return_addr }) if return_addr == callsite_ra
  ));

  let res = unsafe {
    runtime_native::stackwalk_fp::walk_gc_roots_from_safepoint_context(&ctx_missing_sp, Some(bounds), &stackmaps, |_| {})
  };
  assert!(matches!(
    res,
    Err(WalkError::MissingStackmapSp { return_addr }) if return_addr == callsite_ra
  ));
}

#[cfg(target_arch = "x86_64")]
fn sp_entry_and_sp(callsite_sp: usize) -> (usize, usize) {
  // x86_64: `call` pushes an 8-byte return address.
  (callsite_sp - 8, callsite_sp)
}

#[cfg(target_arch = "aarch64")]
fn sp_entry_and_sp(callsite_sp: usize) -> (usize, usize) {
  // aarch64: `bl` does not push a return address.
  (callsite_sp, callsite_sp)
}

fn align_up(v: usize, align: usize) -> usize {
  (v + (align - 1)) & !(align - 1)
}

unsafe fn write_u64(addr: usize, val: u64) {
  (addr as *mut u64).write_unaligned(val);
}

fn build_stackmaps_with_unknown_stack_size() -> Vec<u8> {
  #[cfg(target_arch = "x86_64")]
  let dwarf_sp: u16 = 7;
  #[cfg(target_arch = "aarch64")]
  let dwarf_sp: u16 = 31;

  let mut out = Vec::new();

  // Header.
  out.push(3); // version
  out.push(0); // reserved0
  out.extend_from_slice(&0u16.to_le_bytes()); // reserved1
  out.extend_from_slice(&1u32.to_le_bytes()); // num_functions
  out.extend_from_slice(&0u32.to_le_bytes()); // num_constants
  out.extend_from_slice(&1u32.to_le_bytes()); // num_records

  // One function record.
  out.extend_from_slice(&0x1000u64.to_le_bytes()); // address
  out.extend_from_slice(&u64::MAX.to_le_bytes()); // stack_size (unknown)
  out.extend_from_slice(&1u64.to_le_bytes()); // record_count

  // One record.
  out.extend_from_slice(&0xabcdef00u64.to_le_bytes()); // patchpoint_id
  out.extend_from_slice(&0x10u32.to_le_bytes()); // instruction_offset
  out.extend_from_slice(&0u16.to_le_bytes()); // reserved
  out.extend_from_slice(&5u16.to_le_bytes()); // num_locations: 3 header + (base, derived)

  // 3 leading constants (statepoint header).
  for _ in 0..3 {
    out.extend_from_slice(&[4, 0]); // Constant, reserved
    out.extend_from_slice(&8u16.to_le_bytes()); // size
    out.extend_from_slice(&0u16.to_le_bytes()); // dwarf_reg
    out.extend_from_slice(&0u16.to_le_bytes()); // reserved
    out.extend_from_slice(&0i32.to_le_bytes()); // small const
  }

  // base: Indirect [SP + 0]
  out.extend_from_slice(&[3, 0]); // Indirect, reserved
  out.extend_from_slice(&8u16.to_le_bytes()); // size
  out.extend_from_slice(&dwarf_sp.to_le_bytes()); // dwarf_reg (SP)
  out.extend_from_slice(&0u16.to_le_bytes()); // reserved
  out.extend_from_slice(&0i32.to_le_bytes()); // offset

  // derived: Indirect [SP + 8]
  out.extend_from_slice(&[3, 0]); // Indirect, reserved
  out.extend_from_slice(&8u16.to_le_bytes()); // size
  out.extend_from_slice(&dwarf_sp.to_le_bytes()); // dwarf_reg (SP)
  out.extend_from_slice(&0u16.to_le_bytes()); // reserved
  out.extend_from_slice(&8i32.to_le_bytes()); // offset

  // Align to 8.
  while out.len() % 8 != 0 {
    out.push(0);
  }

  // Live-out header: (padding, num_live_outs). For tests we keep both 0.
  out.extend_from_slice(&0u16.to_le_bytes());
  out.extend_from_slice(&0u16.to_le_bytes());

  // Align to 8.
  while out.len() % 8 != 0 {
    out.push(0);
  }

  out
}
