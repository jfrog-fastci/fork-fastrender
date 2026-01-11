use runtime_native::stackmaps::{Location, StackMap, StackMapRecord, StackSizeRecord};
use runtime_native::statepoint_verify::{
  verify_statepoint_stackmap, DwarfArch, VerifyMode, VerifyStatepointOptions, LLVM_STATEPOINT_PATCHPOINT_ID,
};

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
  assert!(msg.contains("offset=0"));
}

#[test]
fn verifier_rejects_nonzero_flags_header() {
  let stackmap = StackMap {
    version: 3,
    functions: vec![StackSizeRecord {
      address: 0x1000,
      stack_size: 32,
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

  let err = verify_statepoint_stackmap(
    &stackmap,
    VerifyStatepointOptions {
      arch: DwarfArch::X86_64,
      mode: VerifyMode::StatepointsOnly,
    },
  )
  .unwrap_err();
  assert_eq!(err.location_index, Some(1));
  assert!(err.message.contains("flags=0"));
}

#[test]
fn verifier_rejects_deopt_operands() {
  let stackmap = StackMap {
    version: 3,
    functions: vec![StackSizeRecord {
      address: 0x1000,
      stack_size: 32,
      record_count: 1,
    }],
    constants: vec![],
    records: vec![StackMapRecord {
      patchpoint_id: LLVM_STATEPOINT_PATCHPOINT_ID,
      instruction_offset: 0,
      locations: vec![
        Location::Constant { size: 8, value: 0 }, // callconv
        Location::Constant { size: 8, value: 0 }, // flags
        Location::Constant { size: 8, value: 1 }, // deopt_count=1 (unsupported by verifier)
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

  let err = verify_statepoint_stackmap(
    &stackmap,
    VerifyStatepointOptions {
      arch: DwarfArch::X86_64,
      mode: VerifyMode::StatepointsOnly,
    },
  )
  .unwrap_err();
  assert_eq!(err.location_index, Some(2));
  assert!(err.message.contains("deopt"));
}
