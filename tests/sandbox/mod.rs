//! Sandbox security integration tests.
//!
//! These tests validate the OS sandbox boundary (e.g. macOS Seatbelt, Windows AppContainer) rather
//! than renderer behaviour.

#[cfg(target_os = "macos")]
mod macos_seatbelt;

#[cfg(windows)]
mod windows_process_handle_escape;
