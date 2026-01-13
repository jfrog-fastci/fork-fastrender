//! Sandbox security integration tests.
//!
//! These tests validate the OS sandbox boundary (e.g. macOS Seatbelt, Windows AppContainer / job
//! objects) rather than renderer correctness.

#[cfg(target_os = "macos")]
mod macos_seatbelt;

#[cfg(target_os = "macos")]
mod macos_seatbelt_render_smoke;

#[cfg(target_os = "macos")]
mod macos_renderer_sandbox;

#[cfg(target_os = "macos")]
mod macos_sandbox_exec;

#[cfg(target_os = "macos")]
mod macos_sandbox_exec_custom_profile;

#[cfg(target_os = "macos")]
mod macos_sandbox_exec_stdio_ipc;

#[cfg(target_os = "macos")]
mod macos_sandbox_fontdb;

#[cfg(target_os = "linux")]
mod linux_landlock;

#[cfg(target_os = "linux")]
mod linux_namespaces;

#[cfg(target_os = "linux")]
mod linux_pdeathsig;

#[cfg(target_os = "linux")]
mod linux_rlimit_core;

#[cfg(target_os = "linux")]
mod linux_seccomp_hardening;

#[cfg(target_os = "linux")]
mod linux_seccomp_hardening_v2;

#[cfg(target_os = "linux")]
mod linux_seccomp_socket_domain;

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

#[cfg(windows)]
mod windows_appcontainer_temp_dir;

#[cfg(windows)]
mod windows_renderer_sandbox_test;

#[cfg(windows)]
mod windows_sandbox_env_sanitization;

#[cfg(windows)]
mod windows_sandbox_appcontainer_spawn;
