#[cfg(target_arch = "x86_64")]
#[test]
fn validate_stackmaps_skips_deopt_operands() {
  use runtime_native::statepoint_verify::LLVM_STATEPOINT_PATCHPOINT_ID;
  use runtime_native::{validate_stackmaps, StackMaps};

  // Minimal stackmap section containing one statepoint record with:
  // - 3 constant header locations (callconv, flags, deopt_count=1)
  // - 1 deopt operand location (Indirect, should be ignored by `validate_stackmaps`)
  // - 1 GC (base, derived) pair
  //
  // Without skipping deopt operands, the non-constant location count would be odd (1 + 2 = 3).
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
  fn align_to_8(out: &mut Vec<u8>) {
    while out.len() % 8 != 0 {
      out.push(0);
    }
  }

  // Header.
  push_u8(&mut bytes, 3); // version
  push_u8(&mut bytes, 0); // reserved0
  push_u16(&mut bytes, 0); // reserved1
  push_u32(&mut bytes, 1); // num_functions
  push_u32(&mut bytes, 0); // num_constants
  push_u32(&mut bytes, 1); // num_records

  // Function record.
  push_u64(&mut bytes, 0x1000); // address
  push_u64(&mut bytes, 32); // stack_size
  push_u64(&mut bytes, 1); // record_count

  // Record header.
  push_u64(&mut bytes, LLVM_STATEPOINT_PATCHPOINT_ID);
  push_u32(&mut bytes, 0); // instruction_offset
  push_u16(&mut bytes, 0); // reserved
  push_u16(&mut bytes, 6); // num_locations

  // Helper: StackMap location entry (12 bytes).
  fn push_loc(out: &mut Vec<u8>, kind: u8, size: u16, dwarf_reg: u16, offset_or_const: i32) {
    out.push(kind);
    out.push(0); // reserved0
    out.extend_from_slice(&size.to_le_bytes());
    out.extend_from_slice(&dwarf_reg.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes()); // reserved1
    out.extend_from_slice(&offset_or_const.to_le_bytes());
  }

  // 3 constant header locations (callconv, flags, deopt_count).
  push_loc(&mut bytes, 4, 8, 0, 0); // callconv
  push_loc(&mut bytes, 4, 8, 0, 0); // flags
  push_loc(&mut bytes, 4, 8, 0, 1); // deopt_count=1

  // Deopt operand location: Indirect [SP + 0].
  push_loc(&mut bytes, 3, 8, 7, 0); // x86_64 DWARF reg 7 = RSP

  // GC pair: base==derived at Indirect [SP + 8].
  push_loc(&mut bytes, 3, 8, 7, 8);
  push_loc(&mut bytes, 3, 8, 7, 8);

  // Live-out header.
  align_to_8(&mut bytes);
  push_u16(&mut bytes, 0); // padding
  push_u16(&mut bytes, 0); // num_live_outs
  align_to_8(&mut bytes);

  let stackmaps = StackMaps::parse(&bytes).expect("parse StackMaps");
  validate_stackmaps(&stackmaps).expect("validate_stackmaps must skip deopt operands");
}

#[cfg(target_arch = "x86_64")]
#[test]
fn validate_stackmaps_allows_sp_based_locations_with_unknown_stack_size() {
  use runtime_native::statepoint_verify::LLVM_STATEPOINT_PATCHPOINT_ID;
  use runtime_native::{validate_stackmaps, StackMaps};

  // Regression test: LLVM may report an "unknown" stack_size (`u64::MAX`) for
  // dynamically-sized frames (e.g. variable-sized `alloca`). The runtime stack
  // walker does not rely on `stack_size` to evaluate SP-based locations (it
  // derives the callsite SP from the callee FP / safepoint context), so
  // `validate_stackmaps` must not reject SP-based pointer locations when the
  // function's stack_size is unknown.
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
  fn align_to_8(out: &mut Vec<u8>) {
    while out.len() % 8 != 0 {
      out.push(0);
    }
  }

  // Header.
  push_u8(&mut bytes, 3); // version
  push_u8(&mut bytes, 0); // reserved0
  push_u16(&mut bytes, 0); // reserved1
  push_u32(&mut bytes, 1); // num_functions
  push_u32(&mut bytes, 0); // num_constants
  push_u32(&mut bytes, 1); // num_records

  // Function record with unknown stack size.
  push_u64(&mut bytes, 0x1000); // address
  push_u64(&mut bytes, u64::MAX); // stack_size (unknown sentinel)
  push_u64(&mut bytes, 1); // record_count

  // Record header.
  push_u64(&mut bytes, LLVM_STATEPOINT_PATCHPOINT_ID);
  push_u32(&mut bytes, 0); // instruction_offset
  push_u16(&mut bytes, 0); // reserved
  push_u16(&mut bytes, 5); // num_locations (3 header consts + 1 pair)

  // Helper: StackMap location entry (12 bytes).
  fn push_loc(out: &mut Vec<u8>, kind: u8, size: u16, dwarf_reg: u16, offset_or_const: i32) {
    out.push(kind);
    out.push(0); // reserved0
    out.extend_from_slice(&size.to_le_bytes());
    out.extend_from_slice(&dwarf_reg.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes()); // reserved1
    out.extend_from_slice(&offset_or_const.to_le_bytes());
  }

  // 3 constant header locations (callconv, flags, deopt_count=0).
  push_loc(&mut bytes, 4, 8, 0, 0); // callconv
  push_loc(&mut bytes, 4, 8, 0, 0); // flags
  push_loc(&mut bytes, 4, 8, 0, 0); // deopt_count=0

  // GC pair: base==derived at Indirect [SP + 8].
  push_loc(&mut bytes, 3, 8, 7, 8); // x86_64 DWARF reg 7 = RSP
  push_loc(&mut bytes, 3, 8, 7, 8);

  // Live-out header.
  align_to_8(&mut bytes);
  push_u16(&mut bytes, 0); // padding
  push_u16(&mut bytes, 0); // num_live_outs
  align_to_8(&mut bytes);

  let stackmaps = StackMaps::parse(&bytes).expect("parse StackMaps");
  validate_stackmaps(&stackmaps)
    .expect("validate_stackmaps must allow SP-based roots with unknown stack_size");
}

#[cfg(target_arch = "x86_64")]
#[test]
fn validate_stackmaps_treats_invalid_statepoint_layout_as_non_statepoint() {
  use runtime_native::{validate_stackmaps, StackMaps};
  // Minimal stackmap section containing one record that:
  // - starts with 3 Constant header locations (so it *resembles* a statepoint), but
  // - has an intentionally invalid `deopt_count` (too large), so it must *not* be treated as a
  //   valid `gc.statepoint` record.
  //
  // `validate_stackmaps` should fall back to validating the non-constant locations as plain pairs,
  // not error out trying to decode the record as a statepoint.
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
  fn align_to_8(out: &mut Vec<u8>) {
    while out.len() % 8 != 0 {
      out.push(0);
    }
  }

  // Header.
  push_u8(&mut bytes, 3); // version
  push_u8(&mut bytes, 0); // reserved0
  push_u16(&mut bytes, 0); // reserved1
  push_u32(&mut bytes, 1); // num_functions
  push_u32(&mut bytes, 0); // num_constants
  push_u32(&mut bytes, 1); // num_records

  // Function record.
  push_u64(&mut bytes, 0x1000); // address
  push_u64(&mut bytes, 32); // stack_size
  push_u64(&mut bytes, 1); // record_count

  // Record header.
  push_u64(&mut bytes, 0x1234); // patchpoint_id (arbitrary)
  push_u32(&mut bytes, 0); // instruction_offset
  push_u16(&mut bytes, 0); // reserved
  push_u16(&mut bytes, 5); // num_locations = 3 header consts + 2 extra locs

  // Helper: StackMap location entry (12 bytes).
  fn push_loc(out: &mut Vec<u8>, kind: u8, size: u16, dwarf_reg: u16, offset_or_const: i32) {
    out.push(kind);
    out.push(0); // reserved0
    out.extend_from_slice(&size.to_le_bytes());
    out.extend_from_slice(&dwarf_reg.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes()); // reserved1
    out.extend_from_slice(&offset_or_const.to_le_bytes());
  }

  // 3 constant header locations (callconv, flags, deopt_count=100).
  push_loc(&mut bytes, 4, 8, 0, 0); // callconv
  push_loc(&mut bytes, 4, 8, 0, 0); // flags
  push_loc(&mut bytes, 4, 8, 0, 100); // deopt_count=100 (invalid: exceeds locations)

  // Two Indirect locations (valid pair under the non-statepoint validation path).
  push_loc(&mut bytes, 3, 8, 7, 0); // [RSP + 0]
  push_loc(&mut bytes, 3, 8, 7, 8); // [RSP + 8]

  // Live-out header.
  align_to_8(&mut bytes);
  push_u16(&mut bytes, 0); // padding
  push_u16(&mut bytes, 0); // num_live_outs
  align_to_8(&mut bytes);

  let stackmaps = StackMaps::parse(&bytes).expect("parse StackMaps");
  validate_stackmaps(&stackmaps).expect("validate_stackmaps must ignore invalid statepoint layout");
}
