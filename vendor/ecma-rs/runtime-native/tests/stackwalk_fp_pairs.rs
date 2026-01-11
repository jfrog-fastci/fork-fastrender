#![cfg(all(target_os = "linux", any(target_arch = "x86_64", target_arch = "aarch64")))]

use runtime_native::arch;
use runtime_native::stackmaps::StackMaps;
use runtime_native::stackwalk::StackBounds;
use runtime_native::statepoint_verify::LLVM_STATEPOINT_PATCHPOINT_ID;

/// Minimal StackMap v3 blob:
/// - one function record
/// - one callsite record keyed by `instruction_offset`
/// - statepoint layout: 3 constant header locations + one (base, derived) pair
fn minimal_statepoint_stackmap(instruction_offset: u32, stack_size: u64) -> Vec<u8> {
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
  fn align_to(out: &mut Vec<u8>, align: usize) {
    while out.len() % align != 0 {
      out.push(0);
    }
  }
  fn push_loc(out: &mut Vec<u8>, kind: u8, size: u16, dwarf_reg: u16, offset: i32) {
    push_u8(out, kind);
    push_u8(out, 0); // reserved0
    push_u16(out, size);
    push_u16(out, dwarf_reg);
    push_u16(out, 0); // reserved1
    push_i32(out, offset);
  }

  let mut bytes = Vec::new();

  // Header.
  push_u8(&mut bytes, 3); // version
  push_u8(&mut bytes, 0); // reserved0
  push_u16(&mut bytes, 0); // reserved1
  push_u32(&mut bytes, 1); // numFunctions
  push_u32(&mut bytes, 0); // numConstants
  push_u32(&mut bytes, 1); // numRecords

  // Function record.
  push_u64(&mut bytes, 0); // address
  push_u64(&mut bytes, stack_size); // stack_size (intentionally not used by the walker)
  push_u64(&mut bytes, 1); // record_count

  // Record.
  push_u64(&mut bytes, LLVM_STATEPOINT_PATCHPOINT_ID); // patchpoint_id (not used for statepoint detection)
  push_u32(&mut bytes, instruction_offset);
  push_u16(&mut bytes, 0); // reserved
  push_u16(&mut bytes, 5); // num_locations

  // 3 constant header locations (callconv, flags, deopt_count).
  push_loc(&mut bytes, 4, 8, 0, 0);
  push_loc(&mut bytes, 4, 8, 0, 0);
  push_loc(&mut bytes, 4, 8, 0, 0);

  // One (base, derived) pair: Indirect [SP + 0], Indirect [SP + 8].
  let sp_reg = runtime_native::stackwalk::DWARF_SP_REG;
  push_loc(&mut bytes, 3, 8, sp_reg, 0);
  push_loc(&mut bytes, 3, 8, sp_reg, 8);

  // Align to 8 before live-out header.
  align_to(&mut bytes, 8);
  push_u16(&mut bytes, 0); // live-out padding
  push_u16(&mut bytes, 0); // num_live_outs
  align_to(&mut bytes, 8);

  bytes
}

#[repr(align(16))]
struct AlignedStack<const N: usize>([usize; N]);

#[test]
fn root_pairs_use_callee_fp_callsite_sp_not_stack_size() {
  // Create a synthetic stack with two frames:
  // - callee_fp: "runtime" frame (current frame for the walker)
  // - caller_fp: "managed" frame with a stackmap entry keyed by `return_address`
  let mut mem = AlignedStack([0usize; 64]);
  let base = mem.0.as_mut_ptr() as usize;
  let hi = base + mem.0.len() * core::mem::size_of::<usize>();

  let callee_fp = base + 8 * core::mem::size_of::<usize>();
  let caller_fp = base + 24 * core::mem::size_of::<usize>();
  let return_address = 0x1234usize;

  let caller_sp = callee_fp + 16;
  let base_slot_addr = caller_sp as *mut usize;
  let derived_slot_addr = (caller_sp + 8) as *mut usize;

  unsafe {
    // Frame records.
    (callee_fp as *mut usize).write(caller_fp);
    (callee_fp as *mut usize).add(1).write(return_address);

    (caller_fp as *mut usize).write(0);
    (caller_fp as *mut usize).add(1).write(0);

    // Dummy pointer values.
    base_slot_addr.write(0xAAA0);
    derived_slot_addr.write(0xAAA8);
  }

  // Intentionally lie about stack_size: the old pair-walker implementation used stack_size to
  // reconstruct SP and would underflow/out-of-bounds here.
  let stackmaps = StackMaps::parse(&minimal_statepoint_stackmap(return_address as u32, 0x1000)).unwrap();

  let bounds = StackBounds::new(base as u64, hi as u64).unwrap();
  let mut seen: Vec<(usize, usize)> = Vec::new();

  unsafe {
    runtime_native::stackwalk_fp::walk_gc_root_pairs_from_fp(
      callee_fp as u64,
      Some(bounds),
      &stackmaps,
      |ra, pairs| {
        assert_eq!(ra as usize, return_address);
        for &(base_slot, derived_slot) in pairs {
          seen.push((base_slot as usize, derived_slot as usize));
        }
      },
    )
    .unwrap();
  }

  assert_eq!(seen, vec![(base_slot_addr as usize, derived_slot_addr as usize)]);
}

#[test]
fn root_pairs_from_safepoint_context_use_ctx_sp() {
  let mut mem = AlignedStack([0usize; 64]);
  let base = mem.0.as_mut_ptr() as usize;
  let hi = base + mem.0.len() * core::mem::size_of::<usize>();

  let callee_fp = base + 8 * core::mem::size_of::<usize>();
  let caller_fp = base + 24 * core::mem::size_of::<usize>();
  let return_address = 0x1234usize;

  let caller_sp = callee_fp + 16;
  let base_slot_addr = caller_sp as *mut usize;
  let derived_slot_addr = (caller_sp + 8) as *mut usize;

  unsafe {
    (callee_fp as *mut usize).write(caller_fp);
    (callee_fp as *mut usize).add(1).write(return_address);

    (caller_fp as *mut usize).write(0);
    (caller_fp as *mut usize).add(1).write(0);

    base_slot_addr.write(0xAAA0);
    derived_slot_addr.write(0xAAA8);
  }

  let stackmaps = StackMaps::parse(&minimal_statepoint_stackmap(return_address as u32, 0x1000)).unwrap();
  let bounds = StackBounds::new(base as u64, hi as u64).unwrap();

  #[cfg(target_arch = "x86_64")]
  let sp_entry = caller_sp - arch::WORD_SIZE;
  #[cfg(target_arch = "aarch64")]
  let sp_entry = caller_sp;

  let ctx = arch::SafepointContext {
    sp_entry,
    sp: caller_sp,
    fp: caller_fp,
    ip: return_address,
  };

  let mut seen: Vec<(usize, usize)> = Vec::new();
  unsafe {
    runtime_native::stackwalk_fp::walk_gc_root_pairs_from_safepoint_context(
      &ctx,
      Some(bounds),
      &stackmaps,
      |ra, pairs| {
        assert_eq!(ra as usize, return_address);
        for &(base_slot, derived_slot) in pairs {
          seen.push((base_slot as usize, derived_slot as usize));
        }
      },
    )
    .unwrap();
  }

  assert_eq!(seen, vec![(base_slot_addr as usize, derived_slot_addr as usize)]);
}
