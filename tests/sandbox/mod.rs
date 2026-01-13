//! Sandbox security integration tests.
//!
//! These tests validate the OS sandbox boundary (e.g. Windows AppContainer) rather than renderer
//! behaviour.

#[cfg(windows)]
mod windows_process_handle_escape;

