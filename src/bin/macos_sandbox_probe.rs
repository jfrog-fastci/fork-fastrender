//! macOS Seatbelt sandbox probe tool.
//!
//! The actual implementation lives in `src/bin/macos_sandbox_probe/probe.rs` (macOS-only).
//! This wrapper exists so Cargo can discover the binary target on all platforms without trying to
//! compile/link macOS-only code on non-macOS hosts.

#[cfg(target_os = "macos")]
include!("macos_sandbox_probe/probe.rs");

#[cfg(not(target_os = "macos"))]
fn main() {
  eprintln!("macos_sandbox_probe is only supported on macOS.");
  std::process::exit(2);
}
