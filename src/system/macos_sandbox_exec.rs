//! macOS `sandbox-exec` helpers (debug/legacy).
//!
//! This module exists so CLI tooling can opt into wrapping subprocesses with macOS Seatbelt
//! sandboxing via `sandbox-exec`. The actual implementation lives in
//! [`crate::sandbox::macos_spawn`]; this module re-exports the public helpers.
//!
//! IMPORTANT: `sandbox-exec` is deprecated by Apple and may be removed in future macOS releases.
//! Treat this as a debug/legacy mechanism, not a long-term sandboxing strategy.

pub use crate::sandbox::macos_spawn::{
  macos_use_sandbox_exec_from_env, maybe_wrap_command_with_sandbox_exec,
  wrap_command_with_sandbox_exec, ENV_MACOS_USE_SANDBOX_EXEC,
};

