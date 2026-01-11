use runtime_native::stackmaps::{Location, StackMap, StackMapRecord, StackSize, StackSizeRecord};
use runtime_native::statepoint_verify::{
  verify_statepoint_stackmap, DwarfArch, VerifyMode, VerifyStatepointOptions,
  LLVM_STATEPOINT_PATCHPOINT_ID,
};
use runtime_native::StackMaps;

const STATEPOINT_X86_64: &[u8] = include_bytes!("fixtures/bin/statepoint_x86_64.bin");
const STATEPOINT_AARCH64: &[u8] = include_bytes!("fixtures/bin/statepoint_aarch64.bin");

#[test]
fn statepoint_x86_64_fixture_verifies() {
  let stackmap = StackMap::parse(STATEPOINT_X86_64).unwrap();
  verify_statepoint_stackmap(
    &stackmap,
    VerifyStatepointOptions {
      arch: DwarfArch::X86_64,
      mode: VerifyMode::StatepointsOnly,
    },
  )
  .unwrap();
}

#[test]
fn statepoint_aarch64_fixture_verifies() {
  let stackmap = StackMap::parse(STATEPOINT_AARCH64).unwrap();
  verify_statepoint_stackmap(
    &stackmap,
    VerifyStatepointOptions {
      arch: DwarfArch::AArch64,
      mode: VerifyMode::StatepointsOnly,
    },
  )
  .unwrap();
}

#[test]
fn verifier_rejects_register_locations() {
  let mut bytes = STATEPOINT_X86_64.to_vec();

  const HEADER_SIZE: usize = 16;
  const FUNCTION_RECORD_SIZE: usize = 24;
  const RECORD_HEADER_SIZE: usize = 16;
  const LOCATION_SIZE: usize = 12;

  // Location[3] is the first (base) location after the 3 leading Constant(0)
  // statepoint entries.
  let location3_kind_offset =
    HEADER_SIZE + FUNCTION_RECORD_SIZE + RECORD_HEADER_SIZE + LOCATION_SIZE * 3;
  bytes[location3_kind_offset] = 1; // Register (LLVM stackmap kind)
  // Zero out the old Indirect offset field so the verifier's error message is deterministic.
  bytes[location3_kind_offset + 8..location3_kind_offset + 12].fill(0);

  let stackmap = StackMap::parse(&bytes).unwrap();
  let err = verify_statepoint_stackmap(
    &stackmap,
    VerifyStatepointOptions {
      arch: DwarfArch::X86_64,
      mode: VerifyMode::StatepointsOnly,
    },
  )
  .unwrap_err();

  assert_eq!(err.patchpoint_id, 0xABCDEF00);
  assert_eq!(err.location_index, Some(3));
  let loc = err.location.expect("expected location details for VerifyError");
  assert_eq!(loc.kind, "Register");
  assert_eq!(loc.dwarf_reg, 7);
  assert_eq!(loc.offset, 0);

  let msg = err.to_string();
  assert!(msg.contains("return address"));
  assert!(msg.contains("patchpoint_id=0xabcdef00"));
  assert!(msg.contains("location[3]"));
  assert!(msg.contains("kind=Register"));
  assert!(msg.contains("dwarf_reg=7"));
  // We zero out the old Indirect offset field above so the verifier's error message is
  // deterministic. Keep asserting the offset is surfaced for debugging.
  assert!(msg.contains("offset=0"));
}

#[test]
fn verifier_rejects_register_locations_with_custom_statepoint_id() {
  // LLVM allows overriding the statepoint ID / StackMap patchpoint_id via the
  // `"statepoint-id"` callsite directive. The verifier should still treat such
  // records as statepoints and validate their location kinds.
  let stackmap = StackMap {
    version: 3,
    functions: vec![StackSizeRecord {
      address: 0x1000,
      stack_size: StackSize::Known(32),
      record_count: 1,
    }],
    constants: vec![],
    records: vec![StackMapRecord {
      patchpoint_id: 42,
      instruction_offset: 0,
      locations: vec![
        Location::Constant { size: 8, value: 0 }, // callconv
        Location::Constant { size: 8, value: 0 }, // flags
        Location::Constant { size: 8, value: 0 }, // deopt_count
        // One GC pair; base is invalid (Register) and should be rejected.
        Location::Register {
          size: 8,
          dwarf_reg: 7,
          offset: 0,
        },
        Location::Indirect {
          size: 8,
          dwarf_reg: 7,
          offset: 8,
        },
      ],
      live_outs: vec![],
    }],
  };

  let err = verify_statepoint_stackmap(
    &stackmap,
    VerifyStatepointOptions {
      arch: DwarfArch::X86_64,
      mode: VerifyMode::StatepointsOnly,
    },
  )
  .unwrap_err();
  assert_eq!(err.patchpoint_id, 42);
  assert_eq!(err.location_index, Some(3));
  let loc = err.location.expect("expected location details for VerifyError");
  assert_eq!(loc.kind, "Register");
  assert!(err.message.contains("GC root is held in a register"));
}

#[test]
fn verifier_accepts_nonzero_flags_header() {
  let stackmap = StackMap {
    version: 3,
    functions: vec![StackSizeRecord {
      address: 0x1000,
      stack_size: StackSize::Known(32),
      record_count: 1,
    }],
    constants: vec![],
    records: vec![StackMapRecord {
      patchpoint_id: LLVM_STATEPOINT_PATCHPOINT_ID,
      instruction_offset: 0,
      locations: vec![
        Location::Constant { size: 8, value: 0 }, // callconv
        Location::Constant { size: 8, value: 2 }, // flags (non-zero)
        Location::Constant { size: 8, value: 0 }, // deopt_count
        // One GC pair.
        Location::Indirect {
          size: 8,
          dwarf_reg: 7,
          offset: 8,
        },
        Location::Indirect {
          size: 8,
          dwarf_reg: 7,
          offset: 8,
        },
      ],
      live_outs: vec![],
    }],
  };

  verify_statepoint_stackmap(
    &stackmap,
    VerifyStatepointOptions {
      arch: DwarfArch::X86_64,
      mode: VerifyMode::StatepointsOnly,
    },
  )
  .unwrap();
}

#[test]
fn verifier_accepts_deopt_operands() {
  let stackmap = StackMap {
    version: 3,
    functions: vec![StackSizeRecord {
      address: 0x1000,
      stack_size: StackSize::Known(32),
      record_count: 1,
    }],
    constants: vec![],
    records: vec![StackMapRecord {
      patchpoint_id: LLVM_STATEPOINT_PATCHPOINT_ID,
      instruction_offset: 0,
      locations: vec![
        Location::Constant { size: 8, value: 0 }, // callconv
        Location::Constant { size: 8, value: 0 }, // flags
        Location::Constant { size: 8, value: 1 }, // deopt_count=1
        // Deopt operand location.
        Location::Constant { size: 8, value: 123 },
        // One GC pair.
        Location::Indirect {
          size: 8,
          dwarf_reg: 7,
          offset: 8,
        },
        Location::Indirect {
          size: 8,
          dwarf_reg: 7,
          offset: 8,
        },
      ],
      live_outs: vec![],
    }],
  };

  verify_statepoint_stackmap(
    &stackmap,
    VerifyStatepointOptions {
      arch: DwarfArch::X86_64,
      mode: VerifyMode::StatepointsOnly,
    },
  )
  .unwrap();
}

#[test]
fn verifier_does_not_depend_on_patchpoint_id_constant() {
  let mut stackmap = StackMap::parse(STATEPOINT_X86_64).unwrap();
  assert!(
    !stackmap.records.is_empty(),
    "fixture should contain at least one stackmap record"
  );

  // LLVM allows overriding the per-statepoint ID (stored as `patchpoint_id` in the stackmap
  // record). The verifier must therefore detect statepoints by *layout*, not by a fixed ID.
  stackmap.records[0].patchpoint_id = 42;

  verify_statepoint_stackmap(
    &stackmap,
    VerifyStatepointOptions {
      arch: DwarfArch::X86_64,
      mode: VerifyMode::StatepointsOnly,
    },
  )
  .unwrap();
}

#[test]
fn stackmaps_parse_runs_statepoint_verifier() {
  let mut bytes = if cfg!(target_arch = "x86_64") {
    STATEPOINT_X86_64.to_vec()
  } else if cfg!(target_arch = "aarch64") {
    STATEPOINT_AARCH64.to_vec()
  } else {
    // runtime-native only supports x86_64/aarch64 today.
    return;
  };

  const HEADER_SIZE: usize = 16;
  const FUNCTION_RECORD_SIZE: usize = 24;
  const RECORD_HEADER_SIZE: usize = 16;
  const LOCATION_SIZE: usize = 12;

  let location3_kind_offset =
    HEADER_SIZE + FUNCTION_RECORD_SIZE + RECORD_HEADER_SIZE + LOCATION_SIZE * 3;
  bytes[location3_kind_offset] = 1; // Register (LLVM stackmap kind)
  bytes[location3_kind_offset + 8..location3_kind_offset + 12].fill(0);

  let err = StackMaps::parse(&bytes).unwrap_err();
  match err {
    runtime_native::stackmaps::StackMapError::StatepointVerify(v) => {
      assert_eq!(v.location_index, Some(3));
      let loc = v.location.expect("expected location details for VerifyError");
      assert_eq!(loc.kind, "Register");
      assert!(v.message.contains("GC root is held in a register"));
    }
    other => panic!("expected StatepointVerify error, got {other:?}"),
  }
}
