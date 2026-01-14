//! Renderer process sandboxing.
//!
//! This module is intended for **untrusted renderer processes** in the future multiprocess
//! architecture. The sandbox should be applied **as early as possible during renderer startup**,
//! and critically **before any thread pools spawn**.
//!
//! Minimum supported kernels (Linux):
//! - `seccomp-bpf`: Linux ≥ 3.5 (TSYNC may be rejected with `EINVAL` on older kernels)
//! - `Landlock`: Linux ≥ 5.13 (best-effort; treated as unsupported when unavailable)
//!
//! ## Platform implementations
//!
//! - **Linux**: `seccomp-bpf` with `SECCOMP_FILTER_FLAG_TSYNC` when supported to ensure the filter
//!   is applied to all threads in the process. Older kernels that support seccomp filters but not
//!   TSYNC fall back to installing the filter without TSYNC (see
//!   [`SandboxStatus::AppliedWithoutTsync`]), which requires applying the sandbox before any
//!   additional threads spawn.
//! - **macOS**: renderers can call `sandbox_init(3)` (Seatbelt) in-process. For debugging/legacy,
//!   the browser process can also launch renderers through `/usr/bin/sandbox-exec` (deprecated by
//!   Apple; may be removed in future macOS releases). See [`macos_spawn`] for helpers and
//!   `FASTR_MACOS_USE_SANDBOX_EXEC=1` gating (ignored when sandboxing is disabled via
//!   `FASTR_DISABLE_RENDERER_SANDBOX=1` / `FASTR_RENDERER_SANDBOX=off` /
//!   `FASTR_MACOS_RENDERER_SANDBOX=off`).
//! - **Windows**: renderers are intended to be spawned in an AppContainer (no capabilities) with a
//!   Job Object configured for kill-on-close and active-process limiting, plus handle inheritance
//!   allowlisting (`PROC_THREAD_ATTRIBUTE_HANDLE_LIST`) and process mitigations.
//!   - Defense in depth: when supported, the AppContainer token is hardened by removing the broad
//!     `ALL APPLICATION PACKAGES` group via `PROC_THREAD_ATTRIBUTE_ALL_APPLICATION_PACKAGES_POLICY`.
//!   - Sandbox setup **fails closed by default** (to avoid silent security downgrades).
//!   - Set `FASTR_ALLOW_UNSANDBOXED_RENDERER=1` to opt into restricted-token / unsandboxed fallback
//!     on unsupported hosts (see [`windows`]).
//!
//! The current policy is intentionally small and focused:
//! - deny opening filesystem paths (e.g. `open/openat`)
//! - deny creating/using network sockets (e.g. `socket/connect`)
//! - deny executing new programs (`execve/execveat`)
//!
//! Additional restrictions can be layered over time (namespaces, Landlock, etc.).
//!
//! ## Inherited file descriptors
//!
//! Sandbox policies that block filesystem and network syscalls do not automatically revoke access
//! already granted via inherited file descriptors (e.g. a pre-opened file or socket).
//!
//! When spawning sandboxed renderer subprocesses, prefer using [`set_cloexec_on_fds_except`] as a
//! safe defense-in-depth measure to prevent leaking unrelated file descriptors into the exec'd
//! child process.
//!
//! [`close_fds_except`] is a stronger option (it closes everything not whitelisted), but it can be
//! a footgun when used with `std::process::CommandExt::pre_exec`: `std::process::Command` may have
//! internal `CLOEXEC` pipes used for reporting `exec(2)` failures. Closing those fds inside
//! `pre_exec` can cause the child to abort when `exec` fails. Only use `close_fds_except` if you
//! control the full spawn path and know exactly which internal fds must remain open.
//!
//! ## Developer debugging escape hatch
//!
//! Setting [`ENV_DISABLE_RENDERER_SANDBOX`] to a truthy value disables sandbox installation and
//! returns [`SandboxStatus::DisabledByEnv`]. This is intended for local debugging/bisects only.

use std::ffi::OsStr;
use std::io;
#[cfg(target_os = "linux")]
use std::sync::OnceLock;

/// Environment variable that disables renderer sandboxing entirely.
///
/// This is intended for **developer debugging only** (e.g. to attach debuggers/profilers, or to
/// bisect sandbox-related failures). Production deployments should not set this.
pub const ENV_DISABLE_RENDERER_SANDBOX: &str = "FASTR_DISABLE_RENDERER_SANDBOX";

/// Returns true if [`ENV_DISABLE_RENDERER_SANDBOX`] is set to a truthy value.
///
/// Truthiness matches the semantics used by other `FASTR_*` flags in this repository:
/// - falsy: unset, empty, whitespace, `0`, `false`, `no`, `off` (case-insensitive)
/// - truthy: any other non-empty value
pub fn disable_renderer_sandbox_from_env_value(value: Option<&OsStr>) -> bool {
  let Some(value) = value else {
    return false;
  };
  if value.is_empty() {
    return false;
  }
  let value = value.to_string_lossy();
  let trimmed = value.trim();
  if trimmed.is_empty() {
    return false;
  }
  !matches!(
    trimmed.to_ascii_lowercase().as_str(),
    "0" | "false" | "no" | "off"
  )
}

/// Reads [`ENV_DISABLE_RENDERER_SANDBOX`] from the process environment.
pub fn disable_renderer_sandbox_from_env() -> bool {
  let value = std::env::var_os(ENV_DISABLE_RENDERER_SANDBOX);
  disable_renderer_sandbox_from_env_value(value.as_deref())
}

#[cfg(target_os = "linux")]
fn log_linux_renderer_sandbox_disabled_once() {
  static LOGGED: OnceLock<()> = OnceLock::new();
  LOGGED.get_or_init(|| {
    eprintln!(
      "warning: Linux renderer sandbox is DISABLED (debug escape hatch; INSECURE). \
Set {ENV_DISABLE_RENDERER_SANDBOX}=0/1 to control this."
    );
  });
}

pub mod config;

pub mod fd_sanitizer;

pub mod spawn;

pub use fd_sanitizer::{close_fds_except, set_cloexec_on_fds_except};
use std::env::VarError;
/// Seatbelt (macOS) profile string utilities.
///
/// These helpers are pure string logic and are unit-tested on all platforms.
pub mod seatbelt;

#[cfg(target_os = "linux")]
mod linux_prelude;

#[cfg(target_os = "linux")]
pub mod linux_landlock;

#[cfg(target_os = "linux")]
pub mod linux_namespaces;

#[cfg(target_os = "linux")]
mod linux_seccomp;

// macOS Seatbelt sandbox support lives in `macos.rs`. Keep it behind a cfg so the crate still
// builds on non-macOS targets without linking against `libsandbox`.
#[cfg(target_os = "macos")]
pub mod macos;

#[cfg(target_os = "macos")]
pub mod macos_spawn;

#[cfg(target_os = "windows")]
pub mod windows;
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SandboxStatus {
  /// Sandboxing was not applied because sandboxing was disabled via configuration/environment.
  DisabledByEnv,
  /// Sandboxing was not applied because all sandbox layers were disabled via config.
  DisabledByConfig,
  /// Sandboxing was skipped because `report_only` was enabled.
  ReportOnly,
  /// Sandbox was applied successfully (including cross-thread synchronization when available).
  Applied,
  /// Sandbox was applied, but without `SECCOMP_FILTER_FLAG_TSYNC`.
  ///
  /// When `TSYNC` is unavailable, the seccomp filter is only guaranteed to apply to the calling
  /// thread. Callers **must** apply the sandbox *before* spawning any additional threads to ensure
  /// the entire process is covered.
  AppliedWithoutTsync,
  Unsupported,
}

#[cfg(target_os = "macos")]
fn env_var_truthy(raw: Option<&std::ffi::OsStr>) -> bool {
  let Some(raw) = raw else {
    return false;
  };
  if raw.is_empty() {
    return false;
  }
  let raw = raw.to_string_lossy();
  let trimmed = raw.trim();
  if trimmed.is_empty() {
    return false;
  }
  !matches!(
    trimmed.to_ascii_lowercase().as_str(),
    "0" | "false" | "no" | "off"
  )
}

/// Return `true` if the renderer sandbox is explicitly disabled via the common debug escape hatches.
#[cfg(target_os = "macos")]
fn macos_renderer_sandbox_disabled_via_env() -> bool {
  if env_var_truthy(std::env::var_os(macos::ENV_DISABLE_RENDERER_SANDBOX).as_deref()) {
    return true;
  }
  let Some(raw) = std::env::var_os(macos::ENV_MACOS_RENDERER_SANDBOX) else {
    return false;
  };
  if raw.is_empty() {
    return false;
  }
  let raw = raw.to_string_lossy();
  let trimmed = raw.trim();
  if trimmed.is_empty() {
    return false;
  }
  matches!(
    trimmed.to_ascii_lowercase().as_str(),
    "0" | "false" | "no" | "off"
  )
}

#[cfg(target_os = "macos")]
fn macos_renderer_sandbox_mode_override_via_env() -> Option<MacosSandboxMode> {
  let Some(raw) = std::env::var_os(macos::ENV_MACOS_RENDERER_SANDBOX) else {
    return None;
  };
  if raw.is_empty() {
    return None;
  }
  let raw = raw.to_string_lossy();
  let trimmed = raw.trim();
  if trimmed.is_empty() {
    return None;
  }

  let normalized = trimmed.to_ascii_lowercase().replace('_', "-");
  match normalized.as_str() {
    "pure-computation" | "pure" | "strict" => Some(MacosSandboxMode::Strict),
    "system-fonts" | "fonts" | "relaxed" | "renderer-system-fonts" => {
      Some(MacosSandboxMode::Relaxed)
    }
    _ => None,
  }
}

/// Apply the macOS Seatbelt `pure-computation` sandbox to the current process.
///
/// This is primarily intended for sandboxing untrusted renderer subprocesses. It is a one-way
/// operation: once applied, the sandbox cannot be removed.
///
/// This is only supported on macOS (Seatbelt). On other platforms this returns
/// `io::ErrorKind::Unsupported`.
pub fn apply_pure_computation_sandbox() -> io::Result<()> {
  #[cfg(target_os = "macos")]
  return macos::apply_pure_computation_sandbox().map(|_status| ());

  #[cfg(not(target_os = "macos"))]
  return Err(io::Error::new(
    io::ErrorKind::Unsupported,
    "Seatbelt `pure-computation` sandboxing is only supported on macOS",
  ));
}

#[cfg(all(test, not(target_os = "macos")))]
mod pure_computation_sandbox_tests {
  use super::apply_pure_computation_sandbox;

  #[test]
  fn apply_pure_computation_sandbox_is_unsupported() {
    let err = apply_pure_computation_sandbox().expect_err("expected unsupported platform error");
    assert_eq!(err.kind(), std::io::ErrorKind::Unsupported);
  }
}

/// Network socket policy for sandboxed renderer processes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NetworkPolicy {
  /// Deny `socket(2)` / `socketpair(2)` entirely.
  ///
  /// This is the most conservative setting and is the default for renderer sandboxes unless a
  /// caller explicitly opts into Unix-domain sockets for IPC.
  DenyAllSockets,
  /// Allow Unix-domain sockets (`AF_UNIX`) while denying all other socket families.
  ///
  /// This enables IPC mechanisms that rely on `socketpair(AF_UNIX, ...)` or `socket(AF_UNIX, ...)`
  /// while still preventing direct access to the host network (AF_INET/AF_INET6/etc).
  ///
  /// Note: seccomp cannot reliably restrict `connect(2)`/`bind(2)` by inspecting the `sockaddr`
  /// pointer. The security model relies on denying non-Unix socket *creation* and ensuring the
  /// sandboxed process does not inherit any pre-existing network socket file descriptors.
  AllowUnixSocketsOnly,
}

impl Default for NetworkPolicy {
  fn default() -> Self {
    Self::DenyAllSockets
  }
}

/// Landlock sandbox policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RendererLandlockPolicy {
  /// Do not apply Landlock.
  Disabled,
  /// Best-effort Landlock that attempts to deny filesystem writes globally while still allowing
  /// reads (so dynamic linking continues to work).
  ///
  /// If the running kernel does not support Landlock, this falls back to a no-op.
  RestrictWrites,
}

/// Seccomp sandbox policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RendererSeccompPolicy {
  /// Do not install a seccomp filter.
  Disabled,
  /// Default renderer policy. Currently this blocks obvious "escape hatches" like creating
  /// network sockets.
  RendererDefault,
}
#[derive(Debug, Clone, Copy)]
pub struct RendererSandboxConfig {
  /// Master gate for installing a seccomp syscall filter.
  ///
  /// When `false`, no seccomp filter will be installed even if [`Self::seccomp`] selects a policy.
  ///
  /// Default: enabled on Linux, disabled elsewhere.
  pub enable_seccomp: bool,

  /// Master gate for Landlock-based filesystem sandboxing (Linux-only).
  ///
  /// When `false`, Landlock will not be applied even if [`Self::landlock`] selects a policy.
  ///
  /// Default: enabled on Linux (best-effort; treated as unsupported on older kernels).
  pub enable_landlock: bool,

  /// Close unexpected inherited file descriptors.
  ///
  /// Note: FastRender primarily expects callers to use [`close_fds_except`] at **spawn** time to
  /// preserve stdio + known IPC endpoints. This flag exists to make the expectation explicit for
  /// future renderer process entrypoints.
  ///
  /// Default: enabled.
  pub close_extra_fds: bool,

  /// Debug mode: report sandbox decisions without enforcing them.
  ///
  /// Default: disabled.
  pub report_only: bool,

  pub network_policy: NetworkPolicy,
  pub linux_namespaces: linux_namespaces::LinuxNamespacesConfig,
  /// Address space ceiling (RLIMIT_AS) in bytes. `None` disables the limit.
  pub address_space_limit_bytes: Option<u64>,
  /// File descriptor ceiling (RLIMIT_NOFILE). `None` disables the limit.
  pub nofile_limit: Option<u64>,
  /// Core dump size ceiling (RLIMIT_CORE) in bytes. `None` disables the limit.
  ///
  /// Renderer subprocesses should generally set this to `Some(0)` to ensure no coredumps are
  /// produced from untrusted content.
  pub core_limit_bytes: Option<u64>,
  /// Optional clamp for `RLIMIT_NPROC` (processes/threads per user).
  ///
  /// This is intentionally opt-in: the renderer uses threading internally (rayon, font shaping,
  /// image decode), and `RLIMIT_NPROC` is a per-user limit that can interact poorly with other
  /// processes running under the same uid. Callers that run the renderer under a dedicated uid can
  /// set this to a small value, but it is not applied by default.
  pub nproc_limit: Option<u64>,
  /// Landlock filesystem policy.
  pub landlock: RendererLandlockPolicy,
  /// Seccomp policy.
  pub seccomp: RendererSeccompPolicy,
  /// Force-disable `SECCOMP_FILTER_FLAG_TSYNC` even if the running kernel supports it.
  ///
  /// This is primarily intended for tests so the fallback path can be exercised deterministically
  /// on modern kernels.
  pub force_disable_tsync: bool,
}

impl Default for RendererSandboxConfig {
  fn default() -> Self {
    Self {
      enable_seccomp: cfg!(target_os = "linux"),
      enable_landlock: cfg!(target_os = "linux"),
      close_extra_fds: true,
      report_only: false,
      network_policy: NetworkPolicy::DenyAllSockets,
      linux_namespaces: linux_namespaces::LinuxNamespacesConfig::default(),
      address_space_limit_bytes: None,
      // Keep this higher than typical library/userspace needs while still dramatically smaller than
      // the default `ulimit -n` (often 1024+).
      nofile_limit: Some(256),
      core_limit_bytes: Some(0),
      nproc_limit: None,
      landlock: RendererLandlockPolicy::Disabled,
      seccomp: RendererSeccompPolicy::RendererDefault,
      force_disable_tsync: false,
    }
  }
}

/// Report produced while applying sandbox hardening.
#[derive(Debug, Default, Clone)]
pub struct RendererSandboxReport {
  /// Non-fatal failures while applying sandbox knobs.
  pub warnings: Vec<SandboxWarning>,

  /// `true` when `PR_SET_DUMPABLE=0` was applied successfully.
  pub dumpable_disabled: Option<bool>,

  /// (soft, hard) values observed after applying `RLIMIT_AS`.
  pub rlimit_as: Option<(u64, u64)>,

  /// (soft, hard) values observed after applying `RLIMIT_CORE`.
  pub rlimit_core: Option<(u64, u64)>,

  /// (soft, hard) values observed after applying `RLIMIT_NOFILE`.
  pub rlimit_nofile: Option<(u64, u64)>,

  /// (soft, hard) values observed after applying `RLIMIT_NPROC`.
  pub rlimit_nproc: Option<(u64, u64)>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SandboxWarningKind {
  PrctlDumpable,
  RlimitAs,
  RlimitCore,
  RlimitNofile,
  RlimitNproc,
}

#[derive(Debug, Clone)]
pub struct SandboxWarning {
  pub kind: SandboxWarningKind,
  pub message: String,
}

impl SandboxWarning {
  pub(crate) fn new(kind: SandboxWarningKind, message: impl Into<String>) -> Self {
    Self {
      kind,
      message: message.into(),
    }
  }
}

#[derive(Debug, thiserror::Error)]
pub enum RendererSandboxError {
  #[error("rlimit value {value} for {resource} does not fit platform rlim_t")]
  InvalidRlimitValue { resource: &'static str, value: u64 },
  #[error("too many file descriptors to keep open across exec (max {max}, got {actual})")]
  TooManyKeepFds { max: usize, actual: usize },

  #[error(
    "invalid sandbox env var {var}={value:?}; expected 0|1|true|false|yes|no|on|off"
  )]
  InvalidSandboxEnvVar { var: &'static str, value: String },

  #[cfg(target_os = "macos")]
  #[error("failed to configure macOS sandbox-exec wrapper")]
  MacosSandboxExecWrapFailed {
    #[source]
    source: io::Error,
  },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SeccompInstallRejectedReason {
  /// The kernel rejected the request due to insufficient privileges (often: already sandboxed).
  PermissionDenied,
  /// The kernel rejected the request due to invalid arguments (often: already sandboxed, or
  /// unsupported flags like TSYNC on older kernels).
  InvalidArgument,
}

#[derive(Debug, thiserror::Error)]
pub enum SandboxError {
  #[error("sandboxing is not supported on this platform")]
  UnsupportedPlatform,

  #[error("failed to close unexpected file descriptors during sandbox setup")]
  CloseExtraFdsFailed {
    #[source]
    source: io::Error,
  },

  #[cfg(target_os = "linux")]
  #[error("failed to apply Landlock sandbox")]
  LandlockFailed {
    #[source]
    source: linux_landlock::LandlockError,
  },

  #[cfg(target_os = "linux")]
  #[error("failed to set parent-death signal via prctl(PR_SET_PDEATHSIG)")]
  SetParentDeathSignalFailed {
    #[source]
    source: io::Error,
  },

  #[cfg(target_os = "linux")]
  #[error("failed to set PR_SET_DUMPABLE=0")]
  SetDumpableFailed {
    #[source]
    source: io::Error,
  },

  #[cfg(target_os = "linux")]
  #[error("failed to set RLIMIT_CORE to 0")]
  DisableCoreDumpsFailed {
    #[source]
    source: io::Error,
  },

  #[cfg(target_os = "linux")]
  #[error("failed to enable no_new_privs via prctl(PR_SET_NO_NEW_PRIVS)")]
  EnableNoNewPrivsFailed {
    #[source]
    source: io::Error,
  },

  #[cfg(target_os = "linux")]
  #[error("seccomp filter program too large ({len} instruction(s))")]
  SeccompFilterTooLong { len: usize },

  #[cfg(target_os = "linux")]
  #[error(
    "seccomp filter installation rejected ({reason:?}); process may already be sandboxed (errno {errno})"
  )]
  SeccompInstallRejected {
    reason: SeccompInstallRejectedReason,
    errno: i32,
    #[source]
    source: io::Error,
  },

  #[cfg(target_os = "linux")]
  #[error("seccomp filter installation failed (errno {errno})")]
  SeccompInstallFailed {
    errno: i32,
    #[source]
    source: io::Error,
  },

  // --- Linux preflight diagnostics -------------------------------------------------------------

  /// `prctl(PR_GET_SECCOMP)` failed while probing the current seccomp mode.
  #[cfg(target_os = "linux")]
  #[error("failed to query seccomp mode via prctl(PR_GET_SECCOMP): {source}. {guidance}")]
  GetSeccompModeFailed {
    #[source]
    source: io::Error,
    guidance: String,
  },

  /// The process is already in `SECCOMP_MODE_STRICT`, which is incompatible with seccomp-bpf.
  #[cfg(target_os = "linux")]
  #[error("process is already running under SECCOMP_MODE_STRICT. {guidance}")]
  AlreadyInStrictSeccompMode { guidance: String },

  /// `prctl(PR_GET_NO_NEW_PRIVS)` failed while probing whether unprivileged seccomp filters are
  /// allowed.
  #[cfg(target_os = "linux")]
  #[error("failed to query no_new_privs via prctl(PR_GET_NO_NEW_PRIVS): {source}. {guidance}")]
  GetNoNewPrivsFailed {
    #[source]
    source: io::Error,
    guidance: String,
  },

  /// The `seccomp()` syscall is unavailable on this kernel.
  #[cfg(target_os = "linux")]
  #[error("seccomp syscall is unavailable: {source}. {guidance}")]
  SeccompSyscallUnavailable {
    #[source]
    source: io::Error,
    guidance: String,
  },

  /// The running kernel does not support a required seccomp return action.
  #[cfg(target_os = "linux")]
  #[error("seccomp action {action_name} is not supported by the running kernel. {guidance}")]
  SeccompActionUnavailable {
    action_name: &'static str,
    action: u32,
    guidance: String,
  },

  /// The action availability probe itself failed (e.g. blocked by a container policy).
  #[cfg(target_os = "linux")]
  #[error("failed to probe seccomp action {action_name} via SECCOMP_GET_ACTION_AVAIL: {source}. {guidance}")]
  SeccompGetActionAvailFailed {
    action_name: &'static str,
    action: u32,
    #[source]
    source: io::Error,
    guidance: String,
  },

  #[error("invalid {var}: expected one of 0, 1, strict, relaxed, or off; got {value:?}")]
  InvalidBoolean0Or1 { var: &'static str, value: String },

  #[error(
    "invalid {var}: expected one of pure-computation, no-internet, renderer-default, or a path to an SBPL file; got {value:?}"
  )]
  InvalidMacosSeatbeltProfile { var: &'static str, value: String },

  #[error(
    "failed to read SBPL file {path:?} (from {var}={raw_value:?}); expected one of pure-computation, no-internet, renderer-default, or a path to an existing SBPL file"
  )]
  ReadSeatbeltProfileFailed {
    var: &'static str,
    raw_value: String,
    path: std::path::PathBuf,
    #[source]
    source: io::Error,
  },

  #[error("SBPL profile contains an interior NUL byte (from {var}={raw_value:?})")]
  SeatbeltProfileContainsNul {
    var: &'static str,
    raw_value: String,
  },

  #[cfg(target_os = "macos")]
  #[error("failed to apply macOS Seatbelt sandbox (profile={profile}): {errorbuf}")]
  MacosSeatbeltInitFailed { profile: String, errorbuf: String },
}

fn preflight_status(config: &RendererSandboxConfig, disable_env: Option<&OsStr>) -> Option<SandboxStatus> {
  if disable_renderer_sandbox_from_env_value(disable_env) {
    #[cfg(target_os = "linux")]
    log_linux_renderer_sandbox_disabled_once();
    return Some(SandboxStatus::DisabledByEnv);
  }

  let any_enabled = config.enable_seccomp
    || config.enable_landlock
    || config.close_extra_fds
    || config.linux_namespaces.enabled
    || config.address_space_limit_bytes.is_some()
    || config.core_limit_bytes.is_some()
    || config.nofile_limit.is_some()
    || config.nproc_limit.is_some();
  if !any_enabled {
    return Some(SandboxStatus::DisabledByConfig);
  }

  if config.report_only {
    return Some(SandboxStatus::ReportOnly);
  }

  None
}

impl SandboxError {
  /// Optional guidance string for errors that can offer actionable hints.
  pub fn guidance(&self) -> Option<&str> {
    match self {
      #[cfg(target_os = "linux")]
      SandboxError::GetSeccompModeFailed { guidance, .. }
      | SandboxError::AlreadyInStrictSeccompMode { guidance }
      | SandboxError::GetNoNewPrivsFailed { guidance, .. }
      | SandboxError::SeccompSyscallUnavailable { guidance, .. }
      | SandboxError::SeccompActionUnavailable { guidance, .. }
      | SandboxError::SeccompGetActionAvailFailed { guidance, .. } => Some(guidance),
      _ => None,
    }
  }
}

/// Known seccomp modes returned by `prctl(PR_GET_SECCOMP)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinuxSeccompMode {
  Disabled,
  Strict,
  Filter,
  /// A newer/unknown mode value that this build does not understand.
  Unknown(i32),
}

/// Kernel layout probe returned by `seccomp(SECCOMP_GET_NOTIF_SIZES, ...)`.
///
/// This is *not* required for basic seccomp-bpf filtering today. It is queried opportunistically so
/// we can gate "broker" (seccomp user notification) mode in the future.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(C)]
pub struct SeccompNotifSizes {
  pub seccomp_notif: u16,
  pub seccomp_notif_resp: u16,
  pub seccomp_data: u16,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinuxSandboxPreflight {
  /// Current process seccomp mode.
  pub seccomp_mode: LinuxSeccompMode,
  /// Whether `no_new_privs` is already set for the current process.
  pub no_new_privs: bool,
  /// Kernel support for `SECCOMP_RET_KILL_PROCESS` (required by our filters).
  pub has_kill_process: bool,
  /// Kernel support for `SECCOMP_RET_ERRNO` (required by our filters).
  pub has_errno: bool,
  /// Optional seccomp user notification ABI sizing probe.
  pub notif_sizes: Option<SeccompNotifSizes>,
}

/// Parse `FASTR_RENDERER_*` environment variables and apply the renderer sandbox when enabled.
///
/// This is intended to be called very early in renderer process startup (before spawning threads
/// or initializing libraries that might attempt privileged operations).
pub fn maybe_apply_renderer_sandbox_from_env() -> Result<SandboxStatus, SandboxError> {
  // Ensure the cross-platform debug escape hatch behaves consistently even when callers use the
  // higher-level `FASTR_RENDERER_*` env parsing below. When this is set, return `DisabledByEnv` so
  // callers don't mistake a "no-op apply" for an installed sandbox.
  #[cfg(target_os = "macos")]
  {
    if macos_renderer_sandbox_disabled_via_env() {
      // Reuse the macOS module's one-time warning so insecure runs are not silent.
      let _ = macos::apply_strict_sandbox();
      return Ok(SandboxStatus::DisabledByEnv);
    }
  }

  #[cfg(not(target_os = "macos"))]
  {
    if disable_renderer_sandbox_from_env() {
      #[cfg(target_os = "linux")]
      log_linux_renderer_sandbox_disabled_once();
      return Ok(SandboxStatus::DisabledByEnv);
    }
  }

  let default_enabled = cfg!(target_os = "macos");
  let config = match config::RendererSandboxEnvConfig::from_env(default_enabled) {
    Ok(config) => config,
    Err(err) => {
      eprintln!(
        "fastrender: renderer sandbox configuration error\n\
         error: {err}\n\
         hint: set FASTR_RENDERER_SANDBOX=off (or 0) or FASTR_DISABLE_RENDERER_SANDBOX=1 to disable sandboxing for debugging"
      );
      return Err(err);
    }
  };

  if !config.enabled {
    return Ok(SandboxStatus::DisabledByEnv);
  }

  #[cfg(target_os = "macos")]
  {
    use config::MacosSeatbeltProfileSelection;
    let profile_desc = config.macos_seatbelt_profile.describe();

    let apply_result = match &config.macos_seatbelt_profile {
      MacosSeatbeltProfileSelection::PureComputation => {
        macos::apply_renderer_sandbox(macos::MacosSandboxMode::PureComputation)
      }
      MacosSeatbeltProfileSelection::NoInternet => macos::apply_named_profile("no-internet"),
      MacosSeatbeltProfileSelection::RendererDefault => {
        macos::apply_renderer_sandbox(macos::MacosSandboxMode::RendererSystemFonts)
      }
      MacosSeatbeltProfileSelection::SbplPath { .. } => {
        let sbpl = match config.macos_seatbelt_profile.load_sbpl_source() {
          Ok(sbpl) => sbpl,
          Err(err) => {
            eprintln!(
              "fastrender: failed to load macOS Seatbelt sandbox profile (profile={profile_desc})\n\
               error: {err}\n\
               hint: set FASTR_RENDERER_SANDBOX=off (or 0) or FASTR_DISABLE_RENDERER_SANDBOX=1 to disable sandboxing for debugging"
            );
            return Err(err);
          }
        };
        macos::apply_profile_source_with_home_param(&sbpl)
      }
    };

    if let Err(err) = apply_result {
      // Log enough context for debugging/CI where the sandbox may be intentionally disabled.
      eprintln!(
        "fastrender: failed to apply macOS Seatbelt sandbox (profile={profile_desc})\n\
         errorbuf: {err}\n\
         hint: set FASTR_RENDERER_SANDBOX=off (or 0) or FASTR_DISABLE_RENDERER_SANDBOX=1 to disable sandboxing for debugging"
      );
      return Err(SandboxError::MacosSeatbeltInitFailed {
        profile: profile_desc,
        errorbuf: err.to_string(),
      });
    }
    return Ok(SandboxStatus::Applied);
  }

  #[cfg(not(target_os = "macos"))]
  {
    Ok(SandboxStatus::Unsupported)
  }
}

/// Apply hardening steps that must run before seccomp is installed.
///
/// On Linux, this disables core dumps via `RLIMIT_CORE=0` and sets `PR_SET_DUMPABLE=0` so the
/// renderer does not leak sensitive data via core files.
#[cfg(target_os = "linux")]
pub fn apply_renderer_sandbox_prelude() -> Result<(), SandboxError> {
  linux_set_parent_death_signal()
    .map_err(|source| SandboxError::SetParentDeathSignalFailed { source })?;
  let mut report = RendererSandboxReport::default();
  linux_hardening::apply_linux_hardening(&RendererSandboxConfig::default(), &mut report);
  Ok(())
}

/// On Linux, set `PR_SET_PDEATHSIG` so the current process is killed if its parent dies.
///
/// - Uses `SIGKILL` for reliability.
/// - Immediately checks `getppid()`: if it is `1`, the parent already died and the process exits.
///
/// On non-Linux platforms this is a no-op.
pub fn linux_set_parent_death_signal() -> io::Result<()> {
  #[cfg(target_os = "linux")]
  {
    return linux_prelude::linux_set_parent_death_signal();
  }

  #[cfg(not(target_os = "linux"))]
  {
    Ok(())
  }
}

/// Apply the renderer sandbox for the current process.
///
/// Call this during renderer startup, before spawning any thread pools.
///
/// On Linux, the sandbox is process-wide and uses `SECCOMP_FILTER_FLAG_TSYNC` to apply to all
/// threads when the running kernel supports it. When TSYNC is unavailable, the sandbox still
/// installs successfully but returns [`SandboxStatus::AppliedWithoutTsync`] and must be applied
/// before any threads are spawned.
pub fn apply_renderer_sandbox(
  config: RendererSandboxConfig,
) -> Result<SandboxStatus, SandboxError> {
  Ok(apply_renderer_sandbox_with_report(config)?.0)
}

/// Apply the renderer sandbox for the current process, returning a report of best-effort hardening
/// steps.
///
/// This is identical to [`apply_renderer_sandbox`] but preserves non-fatal hardening failures (e.g.
/// rlimit clamps) in the returned report.
pub fn apply_renderer_sandbox_with_report(
  config: RendererSandboxConfig,
) -> Result<(SandboxStatus, RendererSandboxReport), SandboxError> {
  let disable_env = std::env::var_os(ENV_DISABLE_RENDERER_SANDBOX);
  apply_renderer_sandbox_inner(config, disable_env.as_deref())
}

fn apply_renderer_sandbox_inner(
  config: RendererSandboxConfig,
  disable_env: Option<&OsStr>,
) -> Result<(SandboxStatus, RendererSandboxReport), SandboxError> {
  if let Some(status) = preflight_status(&config, disable_env) {
    return Ok((status, RendererSandboxReport::default()));
  }

  let mut report = RendererSandboxReport::default();

  #[cfg(target_os = "linux")]
  {
    // Ensure the renderer is killed if its parent disappears.
    //
    // `linux_seccomp::apply_renderer_sandbox_linux` also sets this, but we apply it here so the
    // hardening layers still get PDEATHSIG even when `seccomp` is disabled.
    linux_set_parent_death_signal().map_err(|source| SandboxError::SetParentDeathSignalFailed {
      source,
    })?;

    // Best-effort defense-in-depth: isolate networking via a new network namespace when permitted.
    // This must run before seccomp, since the seccomp filter may block `unshare(2)`.
    let _ = linux_namespaces::apply_namespaces(config.linux_namespaces);

    linux_hardening::apply_linux_hardening(&config, &mut report);

    if config.close_extra_fds {
      // Close unexpected inherited fds while keeping stdio and the inherited IPC socket.
      //
      // The multiprocess IPC bootstrap helper (`ipc::bootstrap::spawn_child_with_ipc`) duplicates
      // the IPC endpoint to FD 3 before exec. Preserve it by default.
      close_fds_except(&[0, 1, 2, 3]).map_err(|source| SandboxError::CloseExtraFdsFailed { source })?;
    }

    if config.enable_landlock {
      match config.landlock {
        RendererLandlockPolicy::Disabled => {}
        RendererLandlockPolicy::RestrictWrites => {
          // Best-effort Landlock: deny filesystem writes while allowing reads (so pre-opened
          // read-only FDs, dynamic linking, etc. remain usable). If unsupported, we still apply
          // seccomp.
          match linux_landlock::apply_restrict_writes() {
            Ok(linux_landlock::LandlockStatus::Applied { .. }) => {}
            Ok(linux_landlock::LandlockStatus::Unsupported { .. }) => {}
            Err(source) => return Err(SandboxError::LandlockFailed { source }),
          }
        }
      }
    }

    let status = if config.enable_seccomp {
      match config.seccomp {
        RendererSeccompPolicy::Disabled => SandboxStatus::Applied,
        RendererSeccompPolicy::RendererDefault => linux_seccomp::apply_renderer_sandbox_linux(config)?,
      }
    } else {
      SandboxStatus::Applied
    };
    return Ok((status, report));
  }

  #[cfg(not(target_os = "linux"))]
  {
    let _ = config;
    Ok((SandboxStatus::Unsupported, report))
  }
}

/// Applies the Linux renderer seccomp denylist without additional sandbox layers.
///
/// This is primarily useful for unit tests and early bring-up of renderer processes where only
/// syscall filtering is desired.
pub fn apply_renderer_seccomp_denylist() -> Result<SandboxStatus, SandboxError> {
  Ok(
    apply_renderer_seccomp_denylist_with_report(RendererSandboxConfig {
      enable_seccomp: true,
      enable_landlock: false,
      close_extra_fds: false,
      report_only: false,
      network_policy: NetworkPolicy::DenyAllSockets,
      ..RendererSandboxConfig::default()
    })?
    .0,
  )
}

/// Applies the Linux renderer seccomp denylist without additional sandbox layers, returning a
/// report of best-effort hardening steps.
pub fn apply_renderer_seccomp_denylist_with_report(
  config: RendererSandboxConfig,
) -> Result<(SandboxStatus, RendererSandboxReport), SandboxError> {
  let disable_env = std::env::var_os(ENV_DISABLE_RENDERER_SANDBOX);
  if disable_renderer_sandbox_from_env_value(disable_env.as_deref()) {
    return Ok((SandboxStatus::DisabledByEnv, RendererSandboxReport::default()));
  }
  if config.report_only {
    return Ok((SandboxStatus::ReportOnly, RendererSandboxReport::default()));
  }
  if !config.enable_seccomp {
    return Ok((SandboxStatus::DisabledByConfig, RendererSandboxReport::default()));
  }

  let mut report = RendererSandboxReport::default();

  #[cfg(target_os = "linux")]
  {
    linux_hardening::apply_linux_hardening(&config, &mut report);
    let status = linux_seccomp::apply_renderer_sandbox_linux(config)?;
    return Ok((status, report));
  }

  #[cfg(not(target_os = "linux"))]
  {
    let _ = config;
    Ok((SandboxStatus::Unsupported, report))
  }
}

/// Linux-only seccomp/`no_new_privs` compatibility probes.
///
/// This helper is intended to run before installing the renderer sandbox so failures are easier to
/// diagnose (kernel too old, seccomp disabled by container policy, etc.).
#[cfg(target_os = "linux")]
pub fn linux_preflight() -> Result<LinuxSandboxPreflight, SandboxError> {
  let raw_seccomp_mode = linux_seccomp::prctl_get_seccomp_mode().map_err(|source| {
    let guidance = guidance_for_prctl_get_seccomp(&source);
    SandboxError::GetSeccompModeFailed { source, guidance }
  })?;

  let seccomp_mode = match raw_seccomp_mode {
    0 => LinuxSeccompMode::Disabled,
    1 => LinuxSeccompMode::Strict,
    2 => LinuxSeccompMode::Filter,
    other => LinuxSeccompMode::Unknown(other),
  };

  if seccomp_mode == LinuxSeccompMode::Strict {
    return Err(SandboxError::AlreadyInStrictSeccompMode {
      guidance: "The process is already running in `SECCOMP_MODE_STRICT`, which does not allow installing a seccomp-bpf filter. This usually indicates an extremely restrictive sandbox or container policy; run without the Linux sandbox or loosen the parent sandbox."
        .to_string(),
    });
  }

  let no_new_privs = linux_seccomp::prctl_get_no_new_privs().map_err(|source| {
    let guidance = guidance_for_prctl_get_no_new_privs(&source);
    SandboxError::GetNoNewPrivsFailed { source, guidance }
  })?;

  // Verify the kernel understands the return actions we plan to use.
  let has_kill_process =
    match linux_seccomp::seccomp_action_avail(linux_seccomp::SECCOMP_RET_KILL_PROCESS) {
      Ok(()) => true,
      Err(err) => {
        return Err(map_seccomp_action_err(
          "KILL_PROCESS",
          linux_seccomp::SECCOMP_RET_KILL_PROCESS,
          err,
        ));
      }
    };
  let has_errno = match linux_seccomp::seccomp_action_avail(linux_seccomp::SECCOMP_RET_ERRNO) {
    Ok(()) => true,
    Err(err) => {
      return Err(map_seccomp_action_err(
        "ERRNO",
        linux_seccomp::SECCOMP_RET_ERRNO,
        err,
      ));
    }
  };

  // This is an optional feature probe. Kernels without seccomp user notification support will
  // error; we ignore the failure for now because basic sandboxing does not rely on this API.
  let notif_sizes = linux_seccomp::seccomp_get_notif_sizes().ok();

  Ok(LinuxSandboxPreflight {
    seccomp_mode,
    no_new_privs,
    has_kill_process,
    has_errno,
    notif_sizes,
  })
}

#[cfg(not(target_os = "linux"))]
pub fn linux_preflight() -> Result<LinuxSandboxPreflight, SandboxError> {
  Err(SandboxError::UnsupportedPlatform)
}
#[cfg(target_os = "linux")]
fn map_seccomp_action_err(action_name: &'static str, action: u32, err: io::Error) -> SandboxError {
  let errno = err.raw_os_error().unwrap_or_default();
  match errno {
    libc::ENOSYS => SandboxError::SeccompSyscallUnavailable {
      source: err,
      guidance: "The `seccomp()` syscall is not available. A kernel older than Linux 3.17 (or one built without CONFIG_SECCOMP) cannot run this sandbox; upgrade the kernel or disable the Linux sandbox."
        .to_string(),
    },
    libc::EOPNOTSUPP => SandboxError::SeccompActionUnavailable {
      action_name,
      action,
      guidance: "The running kernel does not support the requested seccomp return action. For example, `SECCOMP_RET_KILL_PROCESS` requires a relatively recent kernel; upgrade the kernel or adjust the sandbox policy to use an older action."
        .to_string(),
    },
    libc::EPERM => SandboxError::SeccompGetActionAvailFailed {
      action_name,
      action,
      source: err,
      guidance: "The seccomp syscall appears to be blocked (EPERM). This commonly happens in containers that forbid installing seccomp filters, or when a parent sandbox denies `seccomp()`/`prctl()` calls. Adjust the container security policy to allow seccomp filters."
        .to_string(),
    },
    _ => SandboxError::SeccompGetActionAvailFailed {
      action_name,
      action,
      source: err,
      guidance: "Failed to probe seccomp support. If running inside a container or under another sandbox, ensure the seccomp syscall is permitted and the kernel is new enough for seccomp-bpf."
        .to_string(),
    },
  }
}
#[cfg(target_os = "linux")]
fn guidance_for_prctl_get_seccomp(err: &io::Error) -> String {
  let errno = err.raw_os_error().unwrap_or_default();
  match errno {
    libc::EINVAL | libc::ENOSYS => "The kernel does not appear to support seccomp, or seccomp is disabled at build-time (CONFIG_SECCOMP). Linux < 3.5 likely lacks seccomp-bpf filtering support; upgrade the kernel or disable the Linux sandbox."
      .to_string(),
    libc::EPERM => "Querying seccomp mode was blocked (EPERM). This suggests the process is already under a restrictive sandbox that forbids `prctl()`; run outside the sandbox or loosen the container policy."
      .to_string(),
    _ => "Querying seccomp mode failed. If running inside a container, confirm the runtime allows `prctl(PR_GET_SECCOMP)` and seccomp is enabled in the kernel."
      .to_string(),
  }
}

#[cfg(target_os = "linux")]
fn guidance_for_prctl_get_no_new_privs(err: &io::Error) -> String {
  let errno = err.raw_os_error().unwrap_or_default();
  match errno {
    libc::EINVAL | libc::ENOSYS => "The kernel does not support `no_new_privs` (PR_GET_NO_NEW_PRIVS), which is required for unprivileged seccomp filters. Linux < 3.5 likely lacks this feature; upgrade the kernel or disable the Linux sandbox."
      .to_string(),
    libc::EPERM => "Querying `no_new_privs` was blocked (EPERM). This suggests the process is already under a restrictive sandbox; run outside the sandbox or loosen the container policy."
      .to_string(),
    _ => "Querying `no_new_privs` failed. If running inside a container, confirm the runtime allows `prctl(PR_GET_NO_NEW_PRIVS)` and that the kernel supports seccomp-bpf."
      .to_string(),
  }
}

#[cfg(test)]
mod env_override_tests {
  use super::*;

  #[test]
  fn disable_renderer_sandbox_env_parsing() {
    assert!(!disable_renderer_sandbox_from_env_value(None));
    assert!(!disable_renderer_sandbox_from_env_value(Some(OsStr::new(""))));
    assert!(!disable_renderer_sandbox_from_env_value(Some(OsStr::new("  "))));
    assert!(!disable_renderer_sandbox_from_env_value(Some(OsStr::new("0"))));
    assert!(!disable_renderer_sandbox_from_env_value(Some(OsStr::new("false"))));
    assert!(!disable_renderer_sandbox_from_env_value(Some(OsStr::new("off"))));
    assert!(disable_renderer_sandbox_from_env_value(Some(OsStr::new("1"))));
    assert!(disable_renderer_sandbox_from_env_value(Some(OsStr::new("true"))));
    assert!(disable_renderer_sandbox_from_env_value(Some(OsStr::new("yes"))));
    assert!(disable_renderer_sandbox_from_env_value(Some(OsStr::new("anything"))));
  }

  #[test]
  fn apply_renderer_sandbox_returns_disabled_by_env() {
    let config = RendererSandboxConfig::default();
    let (status, _report) =
      apply_renderer_sandbox_inner(config, Some(OsStr::new("1"))).expect("status result");
    assert_eq!(status, SandboxStatus::DisabledByEnv);
  }

  #[test]
  fn apply_renderer_sandbox_returns_disabled_by_config() {
    let (status, _report) = apply_renderer_sandbox_inner(
      RendererSandboxConfig {
        enable_seccomp: false,
        enable_landlock: false,
        close_extra_fds: false,
        report_only: false,
        network_policy: NetworkPolicy::DenyAllSockets,
        address_space_limit_bytes: None,
        nofile_limit: None,
        core_limit_bytes: None,
        nproc_limit: None,
        ..RendererSandboxConfig::default()
      },
      None,
    )
    .expect("status result");
    assert_eq!(status, SandboxStatus::DisabledByConfig);
  }

  #[test]
  fn apply_renderer_sandbox_returns_report_only() {
    let mut config = RendererSandboxConfig::default();
    config.report_only = true;
    let (status, _report) = apply_renderer_sandbox_inner(config, None).expect("status result");
    assert_eq!(status, SandboxStatus::ReportOnly);
  }

  #[test]
  fn default_config_matches_platform_expectations() {
    let config = RendererSandboxConfig::default();
    assert!(config.close_extra_fds);
    assert!(!config.report_only);
    assert_eq!(config.network_policy, NetworkPolicy::DenyAllSockets);

    #[cfg(target_os = "linux")]
    {
      assert!(config.enable_seccomp);
      assert!(config.enable_landlock);
    }

    #[cfg(not(target_os = "linux"))]
    {
      assert!(!config.enable_seccomp);
      assert!(!config.enable_landlock);
    }
  }
}

#[cfg(target_os = "linux")]
mod linux_hardening;

// ============================================================================
// macOS renderer sandbox env API (`FASTR_RENDERER_SANDBOX`)
// ============================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MacosSandboxMode {
  /// Strict sandbox (no filesystem access; no network access).
  Strict,
  /// Relaxed sandbox: still blocks network, but allows read-only system font access.
  Relaxed,
  /// Do not apply a sandbox (debugging only).
  Off,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MacosSandboxSource {
  Default,
  EnvVar,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MacosSandboxNotAppliedReason {
  ModeOff,
  UnsupportedPlatform,
  ApplyFailed { message: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MacosSandboxStatus {
  Applied {
    mode: MacosSandboxMode,
    source: MacosSandboxSource,
  },
  NotApplied {
    mode: MacosSandboxMode,
    source: MacosSandboxSource,
    reason: MacosSandboxNotAppliedReason,
  },
}

impl MacosSandboxStatus {
  pub fn is_applied(&self) -> bool {
    matches!(self, Self::Applied { .. })
  }
}

#[derive(Debug, thiserror::Error)]
pub enum MacosSandboxError {
  #[error("{var} is not valid Unicode")]
  EnvVarNotUnicode { var: &'static str },

  #[error(transparent)]
  Sandbox(#[from] SandboxError),
}

/// Apply a macOS Seatbelt sandbox configuration based on `FASTR_RENDERER_SANDBOX`.
///
/// This is a macOS-focused wrapper that:
/// - selects sane defaults (strict by default on macOS),
/// - returns a structured status that callers can treat as best-effort in dev or fail-closed in prod.
///
/// It also respects the cross-cutting debug escape hatches:
/// - `FASTR_DISABLE_RENDERER_SANDBOX=1`
/// - `FASTR_MACOS_RENDERER_SANDBOX=off`
///
/// And the profile override (while still keeping sandboxing enabled):
/// - `FASTR_MACOS_RENDERER_SANDBOX=pure-computation|system-fonts` (and aliases)
pub fn apply_macos_sandbox_from_env() -> Result<MacosSandboxStatus, MacosSandboxError> {
  // Ensure the common debug escape hatches (used by tests and local debugging) also affect this
  // higher-level macOS-only API. The underlying Seatbelt wrapper in `macos.rs` will skip sandboxing
  // when these are set, but we want to avoid returning `Applied` in that case.
  #[cfg(target_os = "macos")]
  {
    if macos_renderer_sandbox_disabled_via_env() {
      // Reuse the macOS module's one-time warning so insecure runs are not silent.
      let _ = macos::apply_strict_sandbox();
      return Ok(MacosSandboxStatus::NotApplied {
        mode: MacosSandboxMode::Off,
        source: MacosSandboxSource::EnvVar,
        reason: MacosSandboxNotAppliedReason::ModeOff,
      });
    }
  }

  let default_enabled = cfg!(target_os = "macos");

  let sandbox_env = match std::env::var(config::ENV_RENDERER_SANDBOX) {
    Ok(v) => Some(v),
    Err(VarError::NotPresent) => None,
    Err(VarError::NotUnicode(_)) => {
      return Err(MacosSandboxError::EnvVarNotUnicode {
        var: config::ENV_RENDERER_SANDBOX,
      })
    }
  };

  let seatbelt_profile_env = match std::env::var(config::ENV_MACOS_SEATBELT_PROFILE) {
    Ok(v) => Some(v),
    Err(VarError::NotPresent) => None,
    Err(VarError::NotUnicode(_)) => {
      return Err(MacosSandboxError::EnvVarNotUnicode {
        var: config::ENV_MACOS_SEATBELT_PROFILE,
      })
    }
  };
  let sandbox_env_set = sandbox_env.is_some();
  let seatbelt_profile_env_set = seatbelt_profile_env.is_some();
  let mut source = if sandbox_env_set || seatbelt_profile_env_set {
    MacosSandboxSource::EnvVar
  } else {
    MacosSandboxSource::Default
  };

  let mut env = std::collections::HashMap::<String, String>::new();
  if let Some(v) = sandbox_env {
    env.insert(config::ENV_RENDERER_SANDBOX.to_string(), v);
  }
  if let Some(v) = seatbelt_profile_env {
    env.insert(config::ENV_MACOS_SEATBELT_PROFILE.to_string(), v);
  }

  let config = config::RendererSandboxEnvConfig::from_env_map(&env, default_enabled)?;

  let mut mode = if !config.enabled {
    MacosSandboxMode::Off
  } else if matches!(
    config.macos_seatbelt_profile,
    config::MacosSeatbeltProfileSelection::PureComputation
  ) {
    MacosSandboxMode::Strict
  } else {
    MacosSandboxMode::Relaxed
  };
  #[cfg(target_os = "macos")]
  {
    // `FASTR_MACOS_RENDERER_SANDBOX=system-fonts|pure-computation` can override the strict/relaxed
    // selection for the in-process Seatbelt entrypoints. Only apply it when using those built-in
    // profiles (not when a named profile like `no-internet` or a custom SBPL file is selected).
    //
    // If `FASTR_RENDERER_SANDBOX` (or `FASTR_RENDERER_MACOS_SEATBELT_PROFILE`) is explicitly set, it
    // is treated as the authoritative sandbox configuration surface and this legacy override is
    // ignored.
    if !sandbox_env_set
      && !seatbelt_profile_env_set
      && mode != MacosSandboxMode::Off
      && matches!(
        config.macos_seatbelt_profile,
        config::MacosSeatbeltProfileSelection::PureComputation
          | config::MacosSeatbeltProfileSelection::RendererDefault
      )
    {
      if let Some(override_mode) = macos_renderer_sandbox_mode_override_via_env() {
        mode = override_mode;
        source = MacosSandboxSource::EnvVar;
      }
    }
  }

  if mode == MacosSandboxMode::Off {
    return Ok(MacosSandboxStatus::NotApplied {
      mode,
      source,
      reason: MacosSandboxNotAppliedReason::ModeOff,
    });
  }

  #[cfg(not(target_os = "macos"))]
  {
    return Ok(MacosSandboxStatus::NotApplied {
      mode,
      source,
      reason: MacosSandboxNotAppliedReason::UnsupportedPlatform,
    });
  }

  #[cfg(target_os = "macos")]
  {
    use config::MacosSeatbeltProfileSelection;

    let apply_result = match &config.macos_seatbelt_profile {
      MacosSeatbeltProfileSelection::PureComputation => {
        macos::apply_renderer_sandbox(macos::MacosSandboxMode::PureComputation)
      }
      MacosSeatbeltProfileSelection::NoInternet => macos::apply_named_profile("no-internet"),
      MacosSeatbeltProfileSelection::RendererDefault => {
        macos::apply_renderer_sandbox(macos::MacosSandboxMode::RendererSystemFonts)
      }
      MacosSeatbeltProfileSelection::SbplPath { .. } => {
        let sbpl = config.macos_seatbelt_profile.load_sbpl_source()?;
        macos::apply_profile_source_with_home_param(&sbpl)
      }
    };

    match apply_result {
      Ok(()) => Ok(MacosSandboxStatus::Applied { mode, source }),
      Err(err) => Ok(MacosSandboxStatus::NotApplied {
        mode,
        source,
        reason: MacosSandboxNotAppliedReason::ApplyFailed {
          message: err.to_string(),
        },
      }),
    }
  }
}
#[cfg(all(test, target_os = "linux"))]
mod tests {
  use super::*;
  use std::io::{Read, Write};
  use std::net::TcpListener;
  use std::os::unix::net::UnixStream;
  use std::process::Command;

  fn is_seccomp_unsupported_error(err: &SandboxError) -> bool {
    let errno = match err {
      SandboxError::SetDumpableFailed { source }
      | SandboxError::DisableCoreDumpsFailed { source }
      | SandboxError::EnableNoNewPrivsFailed { source } => source.raw_os_error(),
      SandboxError::SeccompInstallRejected { errno, .. } => Some(*errno),
      SandboxError::SeccompInstallFailed { errno, .. } => Some(*errno),
      _ => None,
    };
    matches!(errno, Some(code) if code == libc::ENOSYS || code == libc::EINVAL)
  }

  #[test]
  fn linux_preflight_smoke() {
    let result = linux_preflight();
    match result {
      Ok(report) => {
        assert!(
          !matches!(report.seccomp_mode, LinuxSeccompMode::Unknown(_)),
          "unexpected unknown seccomp mode: {:?}",
          report.seccomp_mode
        );
        assert!(
          report.has_errno,
          "expected SECCOMP_RET_ERRNO to be available when preflight succeeds"
        );
        assert!(
          report.has_kill_process,
          "expected SECCOMP_RET_KILL_PROCESS to be available when preflight succeeds"
        );
        if let Some(sizes) = report.notif_sizes {
          assert!(sizes.seccomp_data > 0);
          assert!(sizes.seccomp_notif > 0);
          assert!(sizes.seccomp_notif_resp > 0);
        }
      }
      Err(err) => {
        assert!(
          !matches!(err, SandboxError::UnsupportedPlatform),
          "linux preflight should not report unsupported platform on Linux"
        );
        let guidance = err.guidance().unwrap_or_default();
        assert!(
          !guidance.is_empty(),
          "expected guidance string to be present for sandbox errors"
        );
      }
    }
  }

  fn get_rlimit(resource: libc::__rlimit_resource_t) -> (u64, u64) {
    let mut current = libc::rlimit {
      rlim_cur: 0,
      rlim_max: 0,
    };
    // SAFETY: `getrlimit` writes to `current` for a valid pointer.
    let rc = unsafe { libc::getrlimit(resource, &mut current) };
    assert_eq!(rc, 0, "getrlimit({resource}) failed");
    (current.rlim_cur as u64, current.rlim_max as u64)
  }

  #[test]
  fn renderer_hardening_sets_rlimits() {
    const CHILD_ENV: &str = "FASTR_TEST_RENDERER_HARDENING_CHILD";
    let is_child = std::env::var_os(CHILD_ENV).is_some();

    if is_child {
      const NOFILE_CAP: u64 = 256;
      let config = RendererSandboxConfig {
        nofile_limit: Some(NOFILE_CAP),
        core_limit_bytes: Some(0),
        ..Default::default()
      };
      let mut report = RendererSandboxReport::default();
      linux_hardening::apply_linux_hardening(&config, &mut report);

      let dumpable = unsafe { libc::prctl(libc::PR_GET_DUMPABLE, 0, 0, 0, 0) };
      assert_eq!(
        dumpable, 0,
        "expected PR_GET_DUMPABLE to be 0 after sandbox hardening"
      );
      assert_eq!(
        report.dumpable_disabled,
        Some(true),
        "expected report.dumpable_disabled to be true"
      );

      let (core_cur, core_max) = get_rlimit(libc::RLIMIT_CORE);
      assert_eq!(
        core_cur, 0,
        "expected RLIMIT_CORE.cur to be 0 after sandbox hardening"
      );
      assert_eq!(
        core_max, 0,
        "expected RLIMIT_CORE.max to be 0 after sandbox hardening"
      );

      let (nofile_cur, nofile_max) = get_rlimit(libc::RLIMIT_NOFILE);
      assert!(
        nofile_cur <= NOFILE_CAP,
        "expected RLIMIT_NOFILE.cur ({nofile_cur}) <= configured cap ({NOFILE_CAP})"
      );
      assert!(
        nofile_max <= NOFILE_CAP,
        "expected RLIMIT_NOFILE.max ({nofile_max}) <= configured cap ({NOFILE_CAP})"
      );
      return;
    }

    // Run the hardening assertions in a child process so the parent test runner is unaffected.
    let exe = std::env::current_exe().expect("current test exe path");
    let test_name = "sandbox::tests::renderer_hardening_sets_rlimits";
    let output = Command::new(exe)
      .env(CHILD_ENV, "1")
      .arg("--exact")
      .arg(test_name)
      .arg("--nocapture")
      .output()
      .expect("spawn child test process");
    assert!(
      output.status.success(),
      "child process should exit successfully (stdout={}, stderr={})",
      String::from_utf8_lossy(&output.stdout),
      String::from_utf8_lossy(&output.stderr)
    );
  }

  #[test]
  fn renderer_seccomp_denylist_blocks_fs_and_network() {
    const CHILD_ENV: &str = "FASTR_TEST_RENDERER_SECCOMP_CHILD";
    let is_child = std::env::var_os(CHILD_ENV).is_some();
    if is_child {
      match apply_renderer_seccomp_denylist() {
        Ok(
          SandboxStatus::Applied | SandboxStatus::AppliedWithoutTsync,
        ) => {}
        Ok(
          SandboxStatus::DisabledByEnv
          | SandboxStatus::DisabledByConfig
          | SandboxStatus::ReportOnly
          | SandboxStatus::Unsupported,
        ) => return,
        Err(err) => {
          if is_seccomp_unsupported_error(&err) {
            return;
          }
          panic!("failed to apply seccomp sandbox in child: {err}");
        }
      }

      // The sandbox is intended to be applied early, before thread pools spawn. Ensure basic
      // thread creation still works after seccomp is installed.
      let thread = std::thread::spawn(|| 1u32 + 1u32);
      assert_eq!(thread.join().expect("join thread"), 2);

      let meta_err =
        std::fs::metadata("/etc/passwd").expect_err("expected /etc/passwd metadata to fail");
      assert_eq!(
        meta_err.raw_os_error(),
        Some(libc::EPERM),
        "expected EPERM for filesystem metadata (got {meta_err:?})"
      );

      let fs_err = std::fs::read("/etc/passwd").expect_err("expected /etc/passwd read to fail");
      assert_eq!(
        fs_err.raw_os_error(),
        Some(libc::EPERM),
        "expected EPERM for filesystem read (got {fs_err:?})"
      );

      let unix_err = UnixStream::pair().expect_err("expected UnixStream::pair to fail");
      assert_eq!(
        unix_err.raw_os_error(),
        Some(libc::EPERM),
        "expected EPERM for Unix socketpair (got {unix_err:?})"
      );

      let net_err = TcpListener::bind("127.0.0.1:0").expect_err("expected bind to fail");
      assert_eq!(
        net_err.raw_os_error(),
        Some(libc::EPERM),
        "expected EPERM for network bind (got {net_err:?})"
      );

      // Optional sanity check: process execution should be blocked (`execve`).
      let err = Command::new("/bin/true")
        .status()
        .expect_err("expected exec to be blocked under seccomp");
      assert_eq!(
        err.raw_os_error(),
        Some(libc::EPERM),
        "expected exec to fail with EPERM (got {err:?})"
      );

      return;
    }

    // Run the sandbox assertions in a child process so the parent test runner is unaffected.
    let exe = std::env::current_exe().expect("current test exe path");
    let test_name = "sandbox::tests::renderer_seccomp_denylist_blocks_fs_and_network";
    let output = Command::new(exe)
      .env(CHILD_ENV, "1")
      // Avoid a large libtest threadpool: when TSYNC is available the sandbox applies to all
      // threads.
      .env("RUST_TEST_THREADS", "1")
      .arg("--exact")
      .arg(test_name)
      .arg("--nocapture")
      .output()
      .expect("spawn child test process");
    assert!(
      output.status.success(),
      "child process should exit successfully (stdout={}, stderr={})",
      String::from_utf8_lossy(&output.stdout),
      String::from_utf8_lossy(&output.stderr)
    );
  }
  #[test]
  fn renderer_seccomp_allow_unix_sockets_only_allows_unix_ipc_but_denies_tcp() {
    const CHILD_ENV: &str = "FASTR_TEST_RENDERER_SECCOMP_UNIX_ONLY_CHILD";
    let is_child = std::env::var_os(CHILD_ENV).is_some();
    if is_child {
      match linux_seccomp::apply_renderer_sandbox_linux(RendererSandboxConfig {
        network_policy: NetworkPolicy::AllowUnixSocketsOnly,
        ..RendererSandboxConfig::default()
      }) {
        Ok(
          SandboxStatus::Applied | SandboxStatus::AppliedWithoutTsync,
        ) => {}
        Ok(
          SandboxStatus::DisabledByEnv
          | SandboxStatus::DisabledByConfig
          | SandboxStatus::ReportOnly
          | SandboxStatus::Unsupported,
        ) => return,
        Err(err) => {
          if is_seccomp_unsupported_error(&err) {
            return;
          }
          panic!("failed to apply seccomp sandbox in child: {err}");
        }
      }

      let (mut a, mut b) = UnixStream::pair().expect("expected UnixStream::pair to succeed");
      a.write_all(b"ping").expect("write to unix socket");
      let mut buf = [0u8; 4];
      b.read_exact(&mut buf).expect("read from unix socket");
      assert_eq!(&buf, b"ping");

      let net_err = TcpListener::bind("127.0.0.1:0").expect_err("expected tcp bind to fail");
      assert_eq!(
        net_err.raw_os_error(),
        Some(libc::EPERM),
        "expected EPERM for network bind (got {net_err:?})"
      );

      return;
    }

    // Run the sandbox assertions in a child process so the parent test runner is unaffected.
    let exe = std::env::current_exe().expect("current test exe path");
    let test_name =
      "sandbox::tests::renderer_seccomp_allow_unix_sockets_only_allows_unix_ipc_but_denies_tcp";
    let output = Command::new(exe)
      .env(CHILD_ENV, "1")
      .env("RUST_TEST_THREADS", "1")
      .arg("--exact")
      .arg(test_name)
      .arg("--nocapture")
      .output()
      .expect("spawn child test process");
    assert!(
      output.status.success(),
      "child process should exit successfully (stdout={}, stderr={})",
      String::from_utf8_lossy(&output.stdout),
      String::from_utf8_lossy(&output.stderr)
    );
  }
}
#[cfg(all(test, target_os = "macos"))]
mod tests {
  #[cfg(target_os = "macos")]
  mod macos {
    use super::super::apply_pure_computation_sandbox;
    use super::super::{
      apply_macos_sandbox_from_env, MacosSandboxMode, MacosSandboxNotAppliedReason,
      MacosSandboxStatus,
    };
    use std::io::Write;
    use std::process::Command;

    #[test]
    fn pure_computation_sandbox_allows_inherited_stdout_pipe() {
      const CHILD_ENV: &str = "FASTR_TEST_SANDBOX_STDOUT_CHILD";
      const SENTINEL: &[u8] = b"fastrender-seatbelt-stdout-ok";

      if std::env::var_os(CHILD_ENV).is_some() {
        apply_pure_computation_sandbox().expect("apply Seatbelt pure-computation sandbox");
        std::io::stdout()
          .write_all(SENTINEL)
          .and_then(|_| std::io::stdout().flush())
          .expect("write sentinel to stdout after sandbox");
        std::process::exit(0);
      }

      let exe = std::env::current_exe().expect("current test exe path");
      let test_name =
        "sandbox::tests::macos::pure_computation_sandbox_allows_inherited_stdout_pipe";
      let output = Command::new(exe)
        .env(CHILD_ENV, "1")
        .arg("--exact")
        .arg(test_name)
        .arg("--nocapture")
        .output()
        .expect("spawn sandbox child process");

      assert!(
        output.status.success(),
        "sandbox child should exit 0 (stdout={}, stderr={})",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
      );

      assert!(
        output
          .stdout
          .windows(SENTINEL.len())
          .any(|window| window == SENTINEL),
        "expected sandbox child to write sentinel to stdout; got stdout={}, stderr={} ",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
      );
    }

    #[test]
  fn apply_macos_sandbox_from_env_respects_debug_disable_escape_hatch() {
      const CHILD_ENV: &str = "FASTR_TEST_APPLY_MACOS_SANDBOX_FROM_ENV_CHILD";

      if std::env::var_os(CHILD_ENV).is_some() {
        // The escape hatch should bypass parsing invalid FASTR_RENDERER_SANDBOX values and should
        // report `NotApplied` rather than claiming the sandbox is installed.
        let status =
          apply_macos_sandbox_from_env().expect("apply_macos_sandbox_from_env should not error");
        match status {
          MacosSandboxStatus::NotApplied { mode, reason, .. } => {
            assert_eq!(mode, MacosSandboxMode::Off);
            assert_eq!(reason, MacosSandboxNotAppliedReason::ModeOff);
          }
          other => panic!("expected NotApplied due to debug escape hatch, got {other:?}"),
        }
        return;
      }

      let exe = std::env::current_exe().expect("current test exe path");
      let test_name =
        "sandbox::tests::macos::apply_macos_sandbox_from_env_respects_debug_disable_escape_hatch";
      let output = Command::new(exe)
        .env(CHILD_ENV, "1")
        // Disable sandboxing via the common escape hatch.
        .env(super::super::macos::ENV_DISABLE_RENDERER_SANDBOX, "1")
        // Set an intentionally invalid value: we should not surface a config parse error when
        // sandboxing is disabled.
        .env(super::super::config::ENV_RENDERER_SANDBOX, "invalid-value")
        .arg("--exact")
        .arg(test_name)
        .arg("--nocapture")
        .output()
        .expect("spawn sandbox-env child process");

      assert!(
        output.status.success(),
        "sandbox-env child should exit successfully (stdout={}, stderr={})",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
      );
    }

    #[test]
    fn apply_macos_sandbox_from_env_reports_system_fonts_override() {
      const CHILD_ENV: &str = "FASTR_TEST_APPLY_MACOS_SANDBOX_OVERRIDE_CHILD";

      if std::env::var_os(CHILD_ENV).is_some() {
        let status =
          apply_macos_sandbox_from_env().expect("apply_macos_sandbox_from_env should not error");
        match status {
          MacosSandboxStatus::Applied { mode, .. } => {
            assert_eq!(mode, MacosSandboxMode::Relaxed);
          }
          other => panic!("expected Applied status, got {other:?}"),
        }
        // Exit immediately so the sandboxed test process doesn't run additional libtest teardown
        // logic under the Seatbelt profile.
        std::process::exit(0);
      }

      let exe = std::env::current_exe().expect("current test exe path");
      let test_name =
        "sandbox::tests::macos::apply_macos_sandbox_from_env_reports_system_fonts_override";
      let output = Command::new(exe)
        .env(CHILD_ENV, "1")
        // Force the relaxed system-fonts profile via the developer override.
        .env(
          super::super::macos::ENV_MACOS_RENDERER_SANDBOX,
          "system-fonts",
        )
        .arg("--exact")
        .arg(test_name)
        .arg("--nocapture")
        .output()
        .expect("spawn sandbox override child process");

      assert!(
        output.status.success(),
        "sandbox override child should exit successfully (stdout={}, stderr={})",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
      );
    }

    #[test]
    fn apply_macos_sandbox_from_env_reports_fonts_alias_override() {
      const CHILD_ENV: &str = "FASTR_TEST_APPLY_MACOS_SANDBOX_OVERRIDE_FONTS_CHILD";

      if std::env::var_os(CHILD_ENV).is_some() {
        let status =
          apply_macos_sandbox_from_env().expect("apply_macos_sandbox_from_env should not error");
        match status {
          MacosSandboxStatus::Applied { mode, .. } => {
            assert_eq!(mode, MacosSandboxMode::Relaxed);
          }
          other => panic!("expected Applied status, got {other:?}"),
        }
        std::process::exit(0);
      }

      let exe = std::env::current_exe().expect("current test exe path");
      let test_name =
        "sandbox::tests::macos::apply_macos_sandbox_from_env_reports_fonts_alias_override";
      let output = Command::new(exe)
        .env(CHILD_ENV, "1")
        // `fonts` is accepted as an alias for `system-fonts`.
        .env(super::super::macos::ENV_MACOS_RENDERER_SANDBOX, "fonts")
        .arg("--exact")
        .arg(test_name)
        .arg("--nocapture")
        .output()
        .expect("spawn sandbox override child process");

      assert!(
        output.status.success(),
        "sandbox override child should exit successfully (stdout={}, stderr={})",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
      );
    }
  }
}
