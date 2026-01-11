#![cfg(all(target_os = "linux", target_pointer_width = "64", target_endian = "little"))]

use runtime_native::StackMapRegistry;
use runtime_native::stackmaps::StackMapRegistryError;

fn build_min_stackmap_blob(function_address: u64, patchpoint_id: u64) -> Vec<u8> {
  let mut out = Vec::new();

  // Header.
  out.push(3); // version
  out.push(0); // reserved0
  out.extend_from_slice(&0u16.to_le_bytes()); // reserved1
  out.extend_from_slice(&1u32.to_le_bytes()); // num_functions
  out.extend_from_slice(&0u32.to_le_bytes()); // num_constants
  out.extend_from_slice(&1u32.to_le_bytes()); // num_records

  // One function record.
  out.extend_from_slice(&function_address.to_le_bytes());
  out.extend_from_slice(&0u64.to_le_bytes()); // stack_size
  out.extend_from_slice(&1u64.to_le_bytes()); // record_count

  // One record with no locations and no live-outs.
  out.extend_from_slice(&patchpoint_id.to_le_bytes());
  out.extend_from_slice(&0u32.to_le_bytes()); // instruction_offset
  out.extend_from_slice(&0u16.to_le_bytes()); // reserved
  out.extend_from_slice(&0u16.to_le_bytes()); // num_locations

  // Live-out header (already 8-byte aligned at this point): padding + num_live_outs=0.
  out.extend_from_slice(&0u16.to_le_bytes());
  out.extend_from_slice(&0u16.to_le_bytes());
  // Align record end to 8.
  while out.len() % 8 != 0 {
    out.push(0);
  }

  out
}

#[test]
fn registry_register_and_unregister_removes_callsites() {
  let blob1 = build_min_stackmap_blob(0x1000, 1);
  let blob2 = build_min_stackmap_blob(0x2000, 2);

  let start1 = blob1.as_ptr();
  let end1 = unsafe { blob1.as_ptr().add(blob1.len()) };
  let start2 = blob2.as_ptr();
  let end2 = unsafe { blob2.as_ptr().add(blob2.len()) };

  let mut reg = StackMapRegistry::default();

  reg.register(start1, end1).expect("register blob1");
  assert!(reg.lookup(0x1000).is_some());
  assert!(reg.lookup(0x2000).is_none());

  reg.register(start2, end2).expect("register blob2");
  assert!(reg.lookup(0x1000).is_some());
  assert!(reg.lookup(0x2000).is_some());

  assert!(reg.unregister(start1));
  assert!(reg.lookup(0x1000).is_none());
  assert!(reg.lookup(0x2000).is_some());

  // Second unregister should be a no-op.
  assert!(!reg.unregister(start1));
}

#[test]
fn registry_register_is_idempotent_for_same_range() {
  let blob = build_min_stackmap_blob(0x1000, 1);
  let start = blob.as_ptr();
  let end = unsafe { blob.as_ptr().add(blob.len()) };

  let mut reg = StackMapRegistry::default();
  reg.register(start, end).expect("register");
  reg.register(start, end).expect("idempotent register");
  assert!(reg.lookup(0x1000).is_some());
}

#[test]
fn registry_register_rejects_different_range_for_same_start() {
  let blob = build_min_stackmap_blob(0x1000, 1);
  let start = blob.as_ptr();
  let end = unsafe { blob.as_ptr().add(blob.len()) };
  let end_short = unsafe { blob.as_ptr().add(blob.len() - 1) };

  let mut reg = StackMapRegistry::default();
  reg.register(start, end).expect("register");

  let err = reg.register(start, end_short).unwrap_err();
  assert!(matches!(
    err,
    StackMapRegistryError::AlreadyRegisteredDifferentRange
  ));
}
