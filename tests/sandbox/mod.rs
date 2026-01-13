//! Sandbox security integration tests.
//!
//! These tests validate the OS sandbox boundary (e.g. macOS Seatbelt, Windows AppContainer / job
//! objects) rather than renderer correctness.

#[cfg(target_os = "macos")]
mod macos_seatbelt;

#[cfg(windows)]
mod windows_process_handle_escape;

#[cfg(windows)]
mod windows_no_child_process;

#[cfg(windows)]
mod windows_renderer_smoke;

#[cfg(windows)]
mod windows_handle_inheritance;

#[cfg(windows)]
mod windows_job_kill_on_close;

#[cfg(windows)]
mod windows_network_denial;
