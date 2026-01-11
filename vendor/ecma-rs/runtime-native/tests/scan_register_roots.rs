use runtime_native::scan::scan_reloc_pairs;
use runtime_native::{relocate_pair, StatepointRootPair, StackMaps};
use stackmap_context::{ThreadContext, DWARF_REG_IP};

fn build_statepoint_stackmaps_with_register_pair(callsite_offsets: &[u32], base_reg: u16, derived_reg: u16) -> Vec<u8> {
  // Minimal StackMap v3 section containing one function record and N callsite records.
  //
  // Each record is shaped like an LLVM 18 statepoint:
  // - 3 Constant header locations (callconv, flags, deopt_count)
  // - followed by one (base, derived) gc-live pair
  //
  // The gc-live pair is encoded as `LocationKind::Register` for both halves.
  let mut out = Vec::new();

  // Header.
  out.push(3); // version
  out.push(0); // reserved0
  out.extend_from_slice(&0u16.to_le_bytes()); // reserved1
  out.extend_from_slice(&1u32.to_le_bytes()); // num_functions
  out.extend_from_slice(&0u32.to_le_bytes()); // num_constants
  out.extend_from_slice(&(callsite_offsets.len() as u32).to_le_bytes()); // num_records

  // One function record.
  out.extend_from_slice(&0x1000u64.to_le_bytes()); // address
  out.extend_from_slice(&40u64.to_le_bytes()); // stack_size (arbitrary >= FP_RECORD_SIZE)
  out.extend_from_slice(&(callsite_offsets.len() as u64).to_le_bytes()); // record_count

  for &instruction_offset in callsite_offsets {
    out.extend_from_slice(&0xabcdef00u64.to_le_bytes()); // patchpoint_id (LLVM statepoint convention)
    out.extend_from_slice(&instruction_offset.to_le_bytes()); // instruction_offset (=> callsite PC=0x1000+off)
    out.extend_from_slice(&0u16.to_le_bytes()); // reserved
    out.extend_from_slice(&5u16.to_le_bytes()); // num_locations (3 header + 2 gc pair)

    // 3 leading constants (statepoint header). Values are irrelevant for this test.
    for _ in 0..3 {
      out.extend_from_slice(&[4, 0]); // Constant, reserved
      out.extend_from_slice(&8u16.to_le_bytes()); // size
      out.extend_from_slice(&0u16.to_le_bytes()); // dwarf_reg
      out.extend_from_slice(&0u16.to_le_bytes()); // reserved
      out.extend_from_slice(&0i32.to_le_bytes()); // small const
    }

    // base: Register
    out.extend_from_slice(&[1, 0]); // Register, reserved
    out.extend_from_slice(&8u16.to_le_bytes()); // size
    out.extend_from_slice(&base_reg.to_le_bytes()); // dwarf_reg
    out.extend_from_slice(&0u16.to_le_bytes()); // reserved
    out.extend_from_slice(&0i32.to_le_bytes()); // offset (unused for Register)

    // derived: Register
    out.extend_from_slice(&[1, 0]); // Register, reserved
    out.extend_from_slice(&8u16.to_le_bytes()); // size
    out.extend_from_slice(&derived_reg.to_le_bytes()); // dwarf_reg
    out.extend_from_slice(&0u16.to_le_bytes()); // reserved
    out.extend_from_slice(&0i32.to_le_bytes()); // offset (unused for Register)

    // Align to 8 before live-out header.
    while out.len() % 8 != 0 {
      out.push(0);
    }

    // Live-out header: u16 Padding; u16 NumLiveOuts (none).
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());

    // Align record to 8.
    while out.len() % 8 != 0 {
      out.push(0);
    }
  }

  out
}

fn reg_slot_ptr(ctx: &mut ThreadContext, dwarf_reg: u16) -> *mut usize {
  #[cfg(target_arch = "x86_64")]
  {
    match dwarf_reg {
      0 => (&mut ctx.rax as *mut u64).cast::<usize>(),
      1 => (&mut ctx.rdx as *mut u64).cast::<usize>(),
      _ => panic!("unexpected dwarf_reg in test: {dwarf_reg}"),
    }
  }

  #[cfg(target_arch = "aarch64")]
  {
    match dwarf_reg {
      0..=30 => ctx.x.as_mut_ptr().wrapping_add(dwarf_reg as usize).cast::<usize>(),
      _ => panic!("unexpected dwarf_reg in test: {dwarf_reg}"),
    }
  }
}

#[test]
fn scan_reloc_pairs_register_roots_point_into_reg_context_for_all_callsites() {
  let bytes = build_statepoint_stackmaps_with_register_pair(&[0x10, 0x20], 0, 1);
  let stackmaps = StackMaps::parse(&bytes).expect("parse synthetic stackmaps");

  let mut regs = ThreadContext::default();
  for callsite_pc in [0x1010u64, 0x1020u64] {
    // Reset to known values for each iteration so we can reuse the same assertions.
    regs.set_dwarf_reg_u64(0, 0x1000).unwrap();
    regs.set_dwarf_reg_u64(1, 0x1010).unwrap(); // derived = base + 0x10
    regs.set_dwarf_reg_u64(DWARF_REG_IP, callsite_pc).unwrap();

    let pairs = scan_reloc_pairs(&mut regs, &stackmaps).expect("scan");
    assert_eq!(pairs.len(), 1);
    let (base_slot, derived_slot) = pairs[0];

    assert_eq!(base_slot, reg_slot_ptr(&mut regs, 0));
    assert_eq!(derived_slot, reg_slot_ptr(&mut regs, 1));

    // Simulate base relocation and derived fixup; this exercises that the slots behave like normal
    // mutable lvalues even when they live in the saved register file.
    unsafe {
      relocate_pair(
        StatepointRootPair {
          base_slot,
          derived_slot,
        },
        |old| old + 0x1000,
      );
    }

    assert_eq!(regs.get_dwarf_reg_u64(0).unwrap(), 0x2000);
    assert_eq!(regs.get_dwarf_reg_u64(1).unwrap(), 0x2010);
  }
}

#[test]
fn stackmaps_parse_rejects_sp_fp_ip_register_roots() {
  let bytes = build_statepoint_stackmaps_with_register_pair(&[0x10], runtime_native::arch::regs::DWARF_REG_SP, 0);
  let err = StackMaps::parse(&bytes).expect_err("expected parse error");
  let msg = format!("{err}");
  assert!(
    msg.contains("forbidden") && msg.contains("DWARF"),
    "unexpected error message: {msg}"
  );
}
