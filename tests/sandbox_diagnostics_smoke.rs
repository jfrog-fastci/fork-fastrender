#![cfg(any(debug_assertions, feature = "sandbox_diagnostics"))]

use std::io;

#[test]
fn sandbox_check_rejects_nul_bytes() {
  let err = fastrender::debug::sandbox::check_operation_allowed("file-read-data\0nope")
    .expect_err("expected interior NUL bytes to be rejected");
  assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
}

#[test]
#[cfg(not(target_os = "macos"))]
fn sandbox_check_is_unsupported_off_macos() {
  let err = fastrender::debug::sandbox::check_operation_allowed("file-read-data")
    .expect_err("expected sandbox_check helper to be unsupported on non-macOS platforms");
  assert_eq!(err.kind(), io::ErrorKind::Unsupported);
}

