//! Helper for spawning a renderer subprocess with sandboxing configured.
//!
//! The goal is to minimize the "unsandboxed window" by installing security
//! restrictions immediately after `fork(2)` and before `execve(2)`.
//!
//! Platform notes:
//! - **Linux**: uses `CommandExt::pre_exec` to install the sandbox in the child after `fork` and
//!   before `exec` (tightest window).
//! - **macOS**: avoids `pre_exec` (unsafe in multithreaded parents). When explicitly enabled via
//!   `FASTR_MACOS_USE_SANDBOX_EXEC=1`, spawns are wrapped in Apple’s deprecated
//!   `/usr/bin/sandbox-exec` so the renderer starts sandboxed.

use crate::sandbox::{RendererSandboxConfig, RendererSandboxError};
use std::process::{Child, Command, Output};

#[cfg(all(unix, target_os = "linux"))]
use std::os::unix::process::CommandExt;
#[cfg(all(unix, target_os = "linux"))]
use std::sync::OnceLock;

#[cfg(all(unix, target_os = "linux"))]
use crate::system::renderer_sandbox as env_sandbox;

/// A configured renderer command that can be spawned, abstracting over platform-specific sandboxing
/// mechanisms.
///
/// On macOS, sandboxing may be applied by wrapping the original program invocation in
/// `/usr/bin/sandbox-exec` (when explicitly enabled via env vars). That requires owning a temporary
/// profile file until `spawn()`, so the configured command is an owned wrapper rather than a
/// modified `std::process::Command`.
#[derive(Debug)]
pub enum RendererSpawnCommand {
  Plain(Command),
  #[cfg(target_os = "macos")]
  SandboxExec(crate::sandbox_exec::SandboxExecCommand),
}

impl RendererSpawnCommand {
  /// Mutable access to the underlying `Command` for configuration (env vars, stdio, etc).
  pub fn command_mut(&mut self) -> &mut Command {
    match self {
      Self::Plain(cmd) => cmd,
      #[cfg(target_os = "macos")]
      Self::SandboxExec(cmd) => cmd.command_mut(),
    }
  }

  /// Spawn the configured command.
  pub fn spawn(&mut self) -> std::io::Result<Child> {
    match self {
      Self::Plain(cmd) => cmd.spawn(),
      #[cfg(target_os = "macos")]
      Self::SandboxExec(cmd) => cmd.spawn(),
    }
  }

  /// Run the configured command to completion and capture its output.
  pub fn output(&mut self) -> std::io::Result<Output> {
    match self {
      Self::Plain(cmd) => cmd.output(),
      #[cfg(target_os = "macos")]
      Self::SandboxExec(cmd) => cmd.output(),
    }
  }
}

/// Configure `cmd` so the spawned renderer process is sandboxed as early as possible.
///
/// On Linux this uses `CommandExt::pre_exec` to run the sandbox setup in the child
/// process right after `fork` and right before `exec`.
///
/// On macOS, this can optionally wrap the spawn in `/usr/bin/sandbox-exec` when
/// `FASTR_MACOS_USE_SANDBOX_EXEC=1` is set. This path is intended for debugging/legacy workflows
/// only; Apple has deprecated `sandbox-exec`.
///
/// ## Safety notes
///
/// The `pre_exec` closure is executed in the child process after a `fork(2)` from
/// a potentially multi-threaded parent. This means the closure must not:
///
/// - allocate (no `Vec`, no `String`, no formatting),
/// - take locks,
/// - touch global state that could be mid-mutation in another thread.
///
/// The implementation below intentionally uses only direct syscalls / libc
/// functions and stack-allocated data.
///
/// Returns a [`RendererSpawnCommand`] wrapper so macOS can keep any `sandbox-exec` temp profile file
/// alive until `spawn()`.
pub fn configure_renderer_command(
  mut cmd: Command,
  config: RendererSandboxConfig,
) -> Result<RendererSpawnCommand, RendererSandboxError> {
  #[cfg(all(unix, target_os = "linux"))]
  {
    let Some(config) = apply_linux_env_overrides(config)? else {
      return Ok(());
    };
    let cfg = LinuxPreExecConfig::try_from_config(config)?;

    // SAFETY: `pre_exec` is unsafe because the closure runs after fork. The
    // closure uses only async-signal-safe syscalls and does not allocate.
    unsafe {
      cmd.pre_exec(move || linux_pre_exec(cfg));
    }
    return Ok(RendererSpawnCommand::Plain(cmd));
  }

  #[cfg(target_os = "macos")]
  {
    // On macOS, avoid `CommandExt::pre_exec` from a multithreaded parent process. When explicitly
    // enabled, wrap the renderer spawn via `sandbox-exec` instead.
    //
    // This is a debug/legacy mechanism: Apple has deprecated `sandbox-exec` and may remove it in
    // future macOS releases.
    let _ = config;
    let wrapped = crate::sandbox::macos_spawn::maybe_wrap_command_with_sandbox_exec(
      &cmd,
      crate::sandbox::macos::RELAXED_SYSTEM_ALLOWLIST_PROFILE,
    )
    .map_err(|source| RendererSandboxError::MacosSandboxExecWrapFailed { source })?;
    return Ok(match wrapped {
      Some(cmd) => RendererSpawnCommand::SandboxExec(cmd),
      None => RendererSpawnCommand::Plain(cmd),
    });
  }

  #[cfg(not(any(all(unix, target_os = "linux"), target_os = "macos")))]
  {
    let _ = config;
    return Ok(RendererSpawnCommand::Plain(cmd));
  }
}

#[cfg(all(unix, target_os = "linux"))]
fn log_linux_renderer_sandbox_disabled_once() {
  static LOGGED: OnceLock<()> = OnceLock::new();
  LOGGED.get_or_init(|| {
    eprintln!(
      "warning: Linux renderer sandbox is DISABLED (debug escape hatch; INSECURE). \
Set FASTR_DISABLE_RENDERER_SANDBOX=0/1 and FASTR_RENDERER_SECCOMP/FASTR_RENDERER_LANDLOCK to control this."
    );
  });
}

#[cfg(all(unix, target_os = "linux"))]
fn apply_linux_env_overrides(
  config: RendererSandboxConfig,
) -> Result<Option<RendererSandboxConfig>, RendererSandboxError> {
  let defaults = env_sandbox::RendererSandboxConfig {
    enabled: true,
    seccomp: !matches!(
      (config.enable_seccomp, config.seccomp),
      (false, _) | (_, crate::sandbox::RendererSeccompPolicy::Disabled)
    ),
    landlock: !matches!(
      (config.enable_landlock, config.landlock),
      (false, _) | (_, crate::sandbox::RendererLandlockPolicy::Disabled)
    ),
    // Not currently applied here; `std::process::Command` uses internal exec-error pipes that make
    // "close all fds except stdio" from `pre_exec` tricky. (A post-exec close_fds layer can be
    // applied by renderer entrypoints that use stdio-only IPC.)
    close_fds: false,
  };

  let env_cfg =
    env_sandbox::RendererSandboxConfig::from_env_with_defaults(defaults).map_err(|err| {
      RendererSandboxError::InvalidSandboxEnvVar {
        var: err.var(),
        value: err.value().to_string(),
      }
    })?;

  if !env_cfg.enabled {
    log_linux_renderer_sandbox_disabled_once();
    return Ok(None);
  }

  // `enable_*` fields are hard toggles: callers can use them to ensure a given layer never runs,
  // even if an environment override would otherwise enable it.
  //
  // Environment overrides are still allowed to *disable* enabled-by-config layers for local
  // debugging.
  let allow_seccomp = config.enable_seccomp;
  let allow_landlock = config.enable_landlock;

  let mut config = config;
  let seccomp_enabled = allow_seccomp && env_cfg.seccomp;
  let landlock_enabled = allow_landlock && env_cfg.landlock;

  config.seccomp = if seccomp_enabled {
    match config.seccomp {
      crate::sandbox::RendererSeccompPolicy::Disabled => {
        crate::sandbox::RendererSeccompPolicy::RendererDefault
      }
      other => other,
    }
  } else {
    crate::sandbox::RendererSeccompPolicy::Disabled
  };

  config.landlock = if landlock_enabled {
    match config.landlock {
      crate::sandbox::RendererLandlockPolicy::Disabled => {
        crate::sandbox::RendererLandlockPolicy::RestrictWrites
      }
      other => other,
    }
  } else {
    crate::sandbox::RendererLandlockPolicy::Disabled
  };

  // Keep the master `enable_*` toggles consistent with the derived policy, while preserving hard
  // disables from the original config (`allow_*`).
  config.enable_seccomp = seccomp_enabled;
  config.enable_landlock = landlock_enabled;

  Ok(Some(config))
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
  use super::*;
  use crate::sandbox::macos::{ENV_DISABLE_RENDERER_SANDBOX, ENV_MACOS_RENDERER_SANDBOX};
  use crate::sandbox::macos_spawn::ENV_MACOS_USE_SANDBOX_EXEC;
  use std::ffi::OsStr;
  use std::path::Path;
  use std::process::Command;

  #[test]
  fn configure_renderer_command_wraps_with_sandbox_exec_when_env_gate_enabled() {
    let _guard = crate::testing::global_test_lock();

    let prev_gate = std::env::var_os(ENV_MACOS_USE_SANDBOX_EXEC);
    let prev_disable = std::env::var_os(ENV_DISABLE_RENDERER_SANDBOX);
    let prev_profile = std::env::var_os(ENV_MACOS_RENDERER_SANDBOX);

    std::env::set_var(ENV_MACOS_USE_SANDBOX_EXEC, "1");
    std::env::remove_var(ENV_DISABLE_RENDERER_SANDBOX);
    std::env::remove_var(ENV_MACOS_RENDERER_SANDBOX);

    if !Path::new("/usr/bin/sandbox-exec").is_file() {
      eprintln!("skipping: /usr/bin/sandbox-exec is missing");
      restore_env(prev_gate, prev_disable, prev_profile);
      return;
    }

    let cmd = Command::new("/usr/bin/true");
    let cmd = configure_renderer_command(cmd, RendererSandboxConfig::default())
      .expect("configure_renderer_command should succeed");
    match cmd {
      RendererSpawnCommand::SandboxExec(cmd) => assert_eq!(
        cmd.get_program(),
        OsStr::new("/usr/bin/sandbox-exec"),
        "expected command to be wrapped under sandbox-exec"
      ),
      RendererSpawnCommand::Plain(_) => panic!("expected sandbox-exec wrapper to be returned"),
    }

    restore_env(prev_gate, prev_disable, prev_profile);
  }

  fn restore_env(
    prev_gate: Option<std::ffi::OsString>,
    prev_disable: Option<std::ffi::OsString>,
    prev_profile: Option<std::ffi::OsString>,
  ) {
    match prev_gate {
      Some(value) => std::env::set_var(ENV_MACOS_USE_SANDBOX_EXEC, value),
      None => std::env::remove_var(ENV_MACOS_USE_SANDBOX_EXEC),
    }
    match prev_disable {
      Some(value) => std::env::set_var(ENV_DISABLE_RENDERER_SANDBOX, value),
      None => std::env::remove_var(ENV_DISABLE_RENDERER_SANDBOX),
    }
    match prev_profile {
      Some(value) => std::env::set_var(ENV_MACOS_RENDERER_SANDBOX, value),
      None => std::env::remove_var(ENV_MACOS_RENDERER_SANDBOX),
    }
  }
}

#[cfg(all(test, unix, target_os = "linux"))]
mod linux_tests {
  use super::*;
  use crate::system::renderer_sandbox::{
    ENV_DISABLE_RENDERER_SANDBOX, ENV_RENDERER_LANDLOCK, ENV_RENDERER_SECCOMP,
  };

  #[test]
  fn linux_env_disable_skips_sandbox() {
    let _guard = crate::testing::global_test_lock();
    let prev_disable = std::env::var_os(ENV_DISABLE_RENDERER_SANDBOX);
    let prev_seccomp = std::env::var_os(ENV_RENDERER_SECCOMP);
    let prev_landlock = std::env::var_os(ENV_RENDERER_LANDLOCK);

    std::env::set_var(ENV_DISABLE_RENDERER_SANDBOX, "1");
    std::env::remove_var(ENV_RENDERER_SECCOMP);
    std::env::remove_var(ENV_RENDERER_LANDLOCK);

    let result = apply_linux_env_overrides(RendererSandboxConfig::default())
      .expect("apply_linux_env_overrides should succeed");
    assert_eq!(result, None);

    restore_env(prev_disable, prev_seccomp, prev_landlock);
  }

  #[test]
  fn linux_env_disables_seccomp() {
    let _guard = crate::testing::global_test_lock();
    let prev_disable = std::env::var_os(ENV_DISABLE_RENDERER_SANDBOX);
    let prev_seccomp = std::env::var_os(ENV_RENDERER_SECCOMP);
    let prev_landlock = std::env::var_os(ENV_RENDERER_LANDLOCK);

    std::env::remove_var(ENV_DISABLE_RENDERER_SANDBOX);
    std::env::set_var(ENV_RENDERER_SECCOMP, "0");
    std::env::remove_var(ENV_RENDERER_LANDLOCK);

    let cfg = apply_linux_env_overrides(RendererSandboxConfig::default())
      .expect("apply_linux_env_overrides should succeed")
      .expect("sandbox should remain enabled");
    assert_eq!(cfg.seccomp, crate::sandbox::RendererSeccompPolicy::Disabled);

    restore_env(prev_disable, prev_seccomp, prev_landlock);
  }

  #[test]
  fn linux_env_enables_landlock() {
    let _guard = crate::testing::global_test_lock();
    let prev_disable = std::env::var_os(ENV_DISABLE_RENDERER_SANDBOX);
    let prev_seccomp = std::env::var_os(ENV_RENDERER_SECCOMP);
    let prev_landlock = std::env::var_os(ENV_RENDERER_LANDLOCK);

    std::env::remove_var(ENV_DISABLE_RENDERER_SANDBOX);
    std::env::remove_var(ENV_RENDERER_SECCOMP);
    std::env::set_var(ENV_RENDERER_LANDLOCK, "1");

    let cfg = apply_linux_env_overrides(RendererSandboxConfig::default())
      .expect("apply_linux_env_overrides should succeed")
      .expect("sandbox should remain enabled");
    assert_eq!(
      cfg.landlock,
      crate::sandbox::RendererLandlockPolicy::RestrictWrites
    );

    restore_env(prev_disable, prev_seccomp, prev_landlock);
  }

  #[test]
  fn linux_env_rejects_invalid_values() {
    let _guard = crate::testing::global_test_lock();
    let prev_disable = std::env::var_os(ENV_DISABLE_RENDERER_SANDBOX);
    let prev_seccomp = std::env::var_os(ENV_RENDERER_SECCOMP);
    let prev_landlock = std::env::var_os(ENV_RENDERER_LANDLOCK);

    std::env::remove_var(ENV_DISABLE_RENDERER_SANDBOX);
    std::env::set_var(ENV_RENDERER_SECCOMP, "maybe");
    std::env::remove_var(ENV_RENDERER_LANDLOCK);

    let err = apply_linux_env_overrides(RendererSandboxConfig::default())
      .expect_err("expected invalid env var to error");
    match err {
      RendererSandboxError::InvalidSandboxEnvVar { var, value } => {
        assert_eq!(var, ENV_RENDERER_SECCOMP);
        assert_eq!(value, "maybe".to_string());
      }
      other => panic!("unexpected error variant: {other:?}"),
    }

    restore_env(prev_disable, prev_seccomp, prev_landlock);
  }

  fn restore_env(
    prev_disable: Option<std::ffi::OsString>,
    prev_seccomp: Option<std::ffi::OsString>,
    prev_landlock: Option<std::ffi::OsString>,
  ) {
    match prev_disable {
      Some(value) => std::env::set_var(ENV_DISABLE_RENDERER_SANDBOX, value),
      None => std::env::remove_var(ENV_DISABLE_RENDERER_SANDBOX),
    }
    match prev_seccomp {
      Some(value) => std::env::set_var(ENV_RENDERER_SECCOMP, value),
      None => std::env::remove_var(ENV_RENDERER_SECCOMP),
    }
    match prev_landlock {
      Some(value) => std::env::set_var(ENV_RENDERER_LANDLOCK, value),
      None => std::env::remove_var(ENV_RENDERER_LANDLOCK),
    }
  }
}

#[cfg(all(unix, target_os = "linux"))]
#[derive(Debug, Clone, Copy)]
struct LinuxPreExecConfig {
  rlimit_as: Option<libc::rlim_t>,
  rlimit_nofile: Option<libc::rlim_t>,
  rlimit_core: Option<libc::rlim_t>,
  rlimit_nproc: Option<libc::rlim_t>,
  linux_namespaces: crate::sandbox::linux_namespaces::LinuxNamespacesConfig,
  network_policy: crate::sandbox::NetworkPolicy,
  landlock: crate::sandbox::RendererLandlockPolicy,
  seccomp: crate::sandbox::RendererSeccompPolicy,
}

#[cfg(all(unix, target_os = "linux"))]
impl LinuxPreExecConfig {
  fn try_from_config(config: RendererSandboxConfig) -> Result<Self, RendererSandboxError> {
    Ok(Self {
      rlimit_as: config
        .address_space_limit_bytes
        .map(|value| to_rlim_t(value, "RLIMIT_AS"))
        .transpose()?,
      rlimit_nofile: config
        .nofile_limit
        .map(|value| to_rlim_t(value, "RLIMIT_NOFILE"))
        .transpose()?,
      rlimit_core: config
        .core_limit_bytes
        .map(|value| to_rlim_t(value, "RLIMIT_CORE"))
        .transpose()?,
      rlimit_nproc: config
        .nproc_limit
        .map(|value| to_rlim_t(value, "RLIMIT_NPROC"))
        .transpose()?,
      linux_namespaces: config.linux_namespaces,
      network_policy: config.network_policy,
      landlock: config.landlock,
      seccomp: config.seccomp,
    })
  }
}

#[cfg(all(unix, target_os = "linux"))]
fn to_rlim_t(value: u64, resource: &'static str) -> Result<libc::rlim_t, RendererSandboxError> {
  value
    .try_into()
    .map_err(|_| RendererSandboxError::InvalidRlimitValue { resource, value })
}

#[cfg(all(unix, target_os = "linux"))]
fn linux_pre_exec(cfg: LinuxPreExecConfig) -> std::io::Result<()> {
  // 0) Parent-death signal: ensure the renderer is killed if its parent disappears. This should run
  // as early as possible to minimize the unsupervised window.
  set_parent_death_signal_sigkill()?;

  // 0b) Make the process non-dumpable (no ptrace/coredumps). This is defense-in-depth for renderer
  // security boundaries.
  //
  // Best-effort: on hosts that already constrain the process (or older kernels / unusual security
  // policies), PR_SET_DUMPABLE may fail. Do not abort spawning the renderer just because this knob
  // could not be applied.
  let _ = set_dumpable_0();

  // 0c) Optional namespace isolation.
  //
  // This is best-effort and may fail on hosts without sufficient privileges (e.g. user namespaces
  // disabled). When it succeeds, it provides defense-in-depth beyond the seccomp filter by ensuring
  // the process starts inside a fresh network namespace where no interfaces are configured.
  let _ = crate::sandbox::linux_namespaces::apply_namespaces(cfg.linux_namespaces);

  // 1) Apply rlimits.
  if let Some(limit) = cfg.rlimit_as {
    // Best-effort: rlimit clamps are defense-in-depth and should not prevent the renderer from
    // starting if the host disallows changes (e.g. already inside a container with restrictive
    // limits).
    let _ = apply_rlimit_hard_ceiling(libc::RLIMIT_AS, limit);
  }
  if let Some(limit) = cfg.rlimit_nofile {
    let _ = apply_rlimit_hard_ceiling(libc::RLIMIT_NOFILE, limit);
  }
  if let Some(limit) = cfg.rlimit_core {
    let _ = apply_rlimit_hard_ceiling(libc::RLIMIT_CORE, limit);
  }
  if let Some(limit) = cfg.rlimit_nproc {
    let _ = apply_rlimit_hard_ceiling(libc::RLIMIT_NPROC, limit);
  }

  // 2) no_new_privs is required before installing seccomp/landlock without caps. When neither is
  // requested, treat it as best-effort defense-in-depth.
  let needs_no_new_privs = matches!(
    cfg.seccomp,
    crate::sandbox::RendererSeccompPolicy::RendererDefault
  ) || matches!(
    cfg.landlock,
    crate::sandbox::RendererLandlockPolicy::RestrictWrites
  );
  if needs_no_new_privs {
    set_no_new_privs()?;
  } else {
    let _ = set_no_new_privs();
  }

  // 3) Optional Landlock.
  match cfg.landlock {
    crate::sandbox::RendererLandlockPolicy::Disabled => {}
    crate::sandbox::RendererLandlockPolicy::RestrictWrites => {
      apply_landlock_restrict_writes_best_effort()?;
    }
  }

  // 4) Install seccomp.
  match cfg.seccomp {
    crate::sandbox::RendererSeccompPolicy::Disabled => {}
    crate::sandbox::RendererSeccompPolicy::RendererDefault => {
      install_seccomp_renderer_default(cfg.network_policy)?;
    }
  }

  Ok(())
}

#[cfg(all(unix, target_os = "linux"))]
fn apply_rlimit_hard_ceiling(
  resource: libc::__rlimit_resource_t,
  requested: libc::rlim_t,
) -> std::io::Result<()> {
  let mut current = libc::rlimit {
    rlim_cur: 0,
    rlim_max: 0,
  };

  // SAFETY: `getrlimit` writes to `current` when the pointer is valid.
  let rc = unsafe { libc::getrlimit(resource, &mut current) };
  if rc != 0 {
    return Err(std::io::Error::last_os_error());
  }

  // Never attempt to raise limits inside the sandbox:
  // - respect the inherited soft limit (rlim_cur),
  // - and never exceed the inherited hard maximum (rlim_max).
  let mut effective = requested;
  if effective > current.rlim_max {
    effective = current.rlim_max;
  }
  if effective > current.rlim_cur {
    effective = current.rlim_cur;
  }

  let new = libc::rlimit {
    rlim_cur: effective,
    rlim_max: effective,
  };

  // SAFETY: `setrlimit` is a process-global syscall. We pass a properly initialized `rlimit`.
  let rc = unsafe { libc::setrlimit(resource, &new) };
  if rc != 0 {
    return Err(std::io::Error::last_os_error());
  }

  Ok(())
}

#[cfg(all(unix, target_os = "linux"))]
fn set_no_new_privs() -> std::io::Result<()> {
  // SAFETY: `prctl` is a direct syscall wrapper.
  let rc = unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) };
  if rc != 0 {
    return Err(std::io::Error::last_os_error());
  }
  Ok(())
}

#[cfg(all(unix, target_os = "linux"))]
fn set_dumpable_0() -> std::io::Result<()> {
  // SAFETY: `prctl(PR_SET_DUMPABLE, ...)` is a direct syscall wrapper.
  let rc = unsafe { libc::prctl(libc::PR_SET_DUMPABLE, 0, 0, 0, 0) };
  if rc != 0 {
    return Err(std::io::Error::last_os_error());
  }
  Ok(())
}

#[cfg(all(unix, target_os = "linux"))]
fn set_parent_death_signal_sigkill() -> std::io::Result<()> {
  // SAFETY: `prctl` is a direct syscall wrapper.
  let rc = unsafe { libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) };
  if rc != 0 {
    return Err(std::io::Error::last_os_error());
  }

  // There is a race: if the parent dies between `fork` and this `prctl`, the child would become an
  // orphan and would not receive the death signal. Checking `getppid() == 1` after setting PDEATHSIG
  // closes that hole in normal process trees.
  if unsafe { libc::getppid() } == 1 {
    // Best-effort self-kill (should not return).
    unsafe {
      libc::raise(libc::SIGKILL);
      libc::_exit(1);
    }
  }

  Ok(())
}

// --- Landlock (best-effort) --------------------------------------------------

#[cfg(all(unix, target_os = "linux"))]
#[repr(C)]
struct LandlockRulesetAttrV1 {
  handled_access_fs: u64,
}

// Syscall numbers for Landlock (see `linux/landlock.h` + per-arch syscall tables).
//
// We intentionally define these ourselves instead of relying on `libc::SYS_*` because some libc
// versions/targets do not expose the Landlock syscall constants.
//
// When the architecture is unknown, we treat Landlock as unsupported (best-effort no-op).
#[cfg(all(
  unix,
  target_os = "linux",
  any(
    target_arch = "x86_64",
    target_arch = "aarch64",
    target_arch = "riscv64"
  )
))]
const SYS_LANDLOCK_CREATE_RULESET: libc::c_long = 444;
#[cfg(all(
  unix,
  target_os = "linux",
  not(any(
    target_arch = "x86_64",
    target_arch = "aarch64",
    target_arch = "riscv64"
  ))
))]
const SYS_LANDLOCK_CREATE_RULESET: libc::c_long = 0;

#[cfg(all(
  unix,
  target_os = "linux",
  any(
    target_arch = "x86_64",
    target_arch = "aarch64",
    target_arch = "riscv64"
  )
))]
const SYS_LANDLOCK_RESTRICT_SELF: libc::c_long = 446;
#[cfg(all(
  unix,
  target_os = "linux",
  not(any(
    target_arch = "x86_64",
    target_arch = "aarch64",
    target_arch = "riscv64"
  ))
))]
const SYS_LANDLOCK_RESTRICT_SELF: libc::c_long = 0;

#[cfg(all(unix, target_os = "linux"))]
const LANDLOCK_ARCH_SUPPORTED: bool = cfg!(any(
  target_arch = "x86_64",
  target_arch = "aarch64",
  target_arch = "riscv64"
));

#[cfg(all(unix, target_os = "linux"))]
fn apply_landlock_restrict_writes_best_effort() -> std::io::Result<()> {
  // Landlock is optional; if the kernel doesn't support it (ENOSYS / EOPNOTSUPP),
  // we treat it as a no-op.
  if !LANDLOCK_ARCH_SUPPORTED {
    return Ok(());
  }

  // These constants come from `linux/landlock.h`.
  const LANDLOCK_ACCESS_FS_WRITE_FILE: u64 = 1 << 1;
  const LANDLOCK_ACCESS_FS_REMOVE_DIR: u64 = 1 << 4;
  const LANDLOCK_ACCESS_FS_REMOVE_FILE: u64 = 1 << 5;
  const LANDLOCK_ACCESS_FS_MAKE_CHAR: u64 = 1 << 6;
  const LANDLOCK_ACCESS_FS_MAKE_DIR: u64 = 1 << 7;
  const LANDLOCK_ACCESS_FS_MAKE_REG: u64 = 1 << 8;
  const LANDLOCK_ACCESS_FS_MAKE_SOCK: u64 = 1 << 9;
  const LANDLOCK_ACCESS_FS_MAKE_FIFO: u64 = 1 << 10;
  const LANDLOCK_ACCESS_FS_MAKE_BLOCK: u64 = 1 << 11;
  const LANDLOCK_ACCESS_FS_MAKE_SYM: u64 = 1 << 12;
  const LANDLOCK_ACCESS_FS_REFER: u64 = 1 << 13;

  // Restrict writes globally while leaving reads unrestricted (dynamic loader friendly).
  let handled_access_fs = LANDLOCK_ACCESS_FS_WRITE_FILE
    | LANDLOCK_ACCESS_FS_REMOVE_DIR
    | LANDLOCK_ACCESS_FS_REMOVE_FILE
    | LANDLOCK_ACCESS_FS_MAKE_CHAR
    | LANDLOCK_ACCESS_FS_MAKE_DIR
    | LANDLOCK_ACCESS_FS_MAKE_REG
    | LANDLOCK_ACCESS_FS_MAKE_SOCK
    | LANDLOCK_ACCESS_FS_MAKE_FIFO
    | LANDLOCK_ACCESS_FS_MAKE_BLOCK
    | LANDLOCK_ACCESS_FS_MAKE_SYM
    | LANDLOCK_ACCESS_FS_REFER;

  let attr = LandlockRulesetAttrV1 { handled_access_fs };

  // SAFETY: `syscall` is used to call Landlock syscalls directly to avoid pulling
  // in additional dependencies. We pass valid pointers and sizes.
  let fd = unsafe {
    libc::syscall(
      SYS_LANDLOCK_CREATE_RULESET,
      &attr as *const LandlockRulesetAttrV1,
      std::mem::size_of::<LandlockRulesetAttrV1>(),
      0,
    )
  } as libc::c_int;

  if fd < 0 {
    let err = std::io::Error::last_os_error();
    match err.raw_os_error() {
      // Landlock is best-effort here. Treat kernels that don't support the syscall (ENOSYS),
      // don't enable the LSM (EOPNOTSUPP), or reject unknown access bits (E2BIG) as "unsupported"
      // rather than failing the entire spawn.
      Some(libc::ENOSYS) | Some(libc::EOPNOTSUPP) | Some(libc::E2BIG) => return Ok(()),
      _ => return Err(err),
    }
  }

  let rc = unsafe { libc::syscall(SYS_LANDLOCK_RESTRICT_SELF, fd, 0) };
  if rc != 0 {
    let err = std::io::Error::last_os_error();
    unsafe {
      libc::close(fd);
    }
    match err.raw_os_error() {
      Some(libc::ENOSYS) | Some(libc::EOPNOTSUPP) | Some(libc::EPERM) => return Ok(()),
      _ => return Err(err),
    }
  }

  // Best-effort close; ignore failures.
  unsafe {
    libc::close(fd);
  }
  Ok(())
}

// --- Seccomp -----------------------------------------------------------------

#[cfg(all(unix, target_os = "linux"))]
fn install_seccomp_renderer_default(
  network_policy: crate::sandbox::NetworkPolicy,
) -> std::io::Result<()> {
  // This is intentionally a small seccomp filter that is safe to install from
  // a `pre_exec` closure (stack-only, no allocations).
  //
  // It focuses on removing network capabilities early while still allowing the
  // process to `execve` the intended renderer binary.

  const SECCOMP_DATA_NR_OFFSET: u32 = 0;
  const SECCOMP_DATA_ARCH_OFFSET: u32 = 4;
  const SECCOMP_DATA_ARG0_OFFSET: u32 = 16;

  // BPF instruction encoding (from `linux/filter.h`).
  const BPF_LD: u16 = 0x00;
  const BPF_W: u16 = 0x00;
  const BPF_ABS: u16 = 0x20;
  const BPF_JMP: u16 = 0x05;
  const BPF_JEQ: u16 = 0x10;
  const BPF_K: u16 = 0x00;
  const BPF_RET: u16 = 0x06;

  const fn bpf_stmt(code: u16, k: u32) -> libc::sock_filter {
    libc::sock_filter {
      code,
      jt: 0,
      jf: 0,
      k,
    }
  }
  const fn bpf_jump(code: u16, k: u32, jt: u8, jf: u8) -> libc::sock_filter {
    libc::sock_filter { code, jt, jf, k }
  }

  // seccomp return values (`linux/seccomp.h`)
  const SECCOMP_RET_KILL_THREAD: u32 = 0x00000000;
  const SECCOMP_RET_ERRNO: u32 = 0x50000000;
  const SECCOMP_RET_ALLOW: u32 = 0x7fff0000;
  const SECCOMP_RET_DATA: u32 = 0x0000ffff;

  let arch = audit_arch();

  // We build the program on the stack; no allocations.
  //
  // Max program size:
  //   - arch check: 3
  //   - load nr: 1
  //   - socket policy: up to 5
  //   - socketpair policy: up to 5
  //   - optional extra denies: up to 2 (connect) -> 2*1
  //   - allow: 1
  // Total <= 17 (plus slack).
  let mut filter: [libc::sock_filter; 64] = [bpf_stmt(0, 0); 64];
  let mut i = 0usize;

  if let Some(arch) = arch {
    filter[i] = bpf_stmt(BPF_LD | BPF_W | BPF_ABS, SECCOMP_DATA_ARCH_OFFSET);
    i += 1;
    // If arch matches, fall through; else jump to kill.
    filter[i] = bpf_jump(BPF_JMP | BPF_JEQ | BPF_K, arch, 0, 1);
    i += 1;
    filter[i] = bpf_stmt(BPF_RET | BPF_K, SECCOMP_RET_KILL_THREAD);
    i += 1;
  }

  filter[i] = bpf_stmt(BPF_LD | BPF_W | BPF_ABS, SECCOMP_DATA_NR_OFFSET);
  i += 1;

  let ret_eperm = SECCOMP_RET_ERRNO | ((libc::EPERM as u32) & SECCOMP_RET_DATA);

  // Network socket policy.
  match network_policy {
    crate::sandbox::NetworkPolicy::DenyAllSockets => {
      // Deny all sockets, including AF_UNIX.
      filter[i] = bpf_jump(BPF_JMP | BPF_JEQ | BPF_K, libc::SYS_socket as u32, 0, 1);
      i += 1;
      filter[i] = bpf_stmt(BPF_RET | BPF_K, ret_eperm);
      i += 1;

      filter[i] = bpf_jump(BPF_JMP | BPF_JEQ | BPF_K, libc::SYS_socketpair as u32, 0, 1);
      i += 1;
      filter[i] = bpf_stmt(BPF_RET | BPF_K, ret_eperm);
      i += 1;

      // Defense-in-depth: deny `connect(2)` so inherited sockets are less useful.
      filter[i] = bpf_jump(BPF_JMP | BPF_JEQ | BPF_K, libc::SYS_connect as u32, 0, 1);
      i += 1;
      filter[i] = bpf_stmt(BPF_RET | BPF_K, ret_eperm);
      i += 1;
    }
    crate::sandbox::NetworkPolicy::AllowUnixSocketsOnly => {
      // Allow `socket(AF_UNIX, ...)` while denying other families.
      filter[i] = bpf_jump(BPF_JMP | BPF_JEQ | BPF_K, libc::SYS_socket as u32, 0, 4);
      i += 1;
      filter[i] = bpf_stmt(BPF_LD | BPF_W | BPF_ABS, SECCOMP_DATA_ARG0_OFFSET);
      i += 1;
      filter[i] = bpf_jump(BPF_JMP | BPF_JEQ | BPF_K, libc::AF_UNIX as u32, 1, 0);
      i += 1;
      filter[i] = bpf_stmt(BPF_RET | BPF_K, ret_eperm);
      i += 1;
      filter[i] = bpf_stmt(BPF_RET | BPF_K, SECCOMP_RET_ALLOW);
      i += 1;

      // Allow `socketpair(AF_UNIX, ...)` while denying other families.
      filter[i] = bpf_jump(BPF_JMP | BPF_JEQ | BPF_K, libc::SYS_socketpair as u32, 0, 4);
      i += 1;
      filter[i] = bpf_stmt(BPF_LD | BPF_W | BPF_ABS, SECCOMP_DATA_ARG0_OFFSET);
      i += 1;
      filter[i] = bpf_jump(BPF_JMP | BPF_JEQ | BPF_K, libc::AF_UNIX as u32, 1, 0);
      i += 1;
      filter[i] = bpf_stmt(BPF_RET | BPF_K, ret_eperm);
      i += 1;
      filter[i] = bpf_stmt(BPF_RET | BPF_K, SECCOMP_RET_ALLOW);
      i += 1;
    }
  }

  filter[i] = bpf_stmt(BPF_RET | BPF_K, SECCOMP_RET_ALLOW);
  i += 1;

  let prog = libc::sock_fprog {
    len: i as u16,
    filter: filter.as_ptr() as *mut libc::sock_filter,
  };

  // SAFETY: `prctl(PR_SET_SECCOMP, ...)` installs the filter. The BPF program is
  // stack allocated and stays alive for the duration of the syscall.
  let rc = unsafe { libc::prctl(libc::PR_SET_SECCOMP, libc::SECCOMP_MODE_FILTER, &prog) };
  if rc != 0 {
    return Err(std::io::Error::last_os_error());
  }
  Ok(())
}

#[cfg(all(unix, target_os = "linux"))]
const fn audit_arch() -> Option<u32> {
  // Values from `linux/audit.h`:
  //   AUDIT_ARCH_* = EM_* | __AUDIT_ARCH_* flags
  #[cfg(target_arch = "x86_64")]
  {
    Some(0xc000003e)
  }
  #[cfg(target_arch = "aarch64")]
  {
    Some(0xc00000b7)
  }
  #[cfg(target_arch = "riscv64")]
  {
    Some(0xc00000f3)
  }
  #[cfg(target_arch = "arm")]
  {
    Some(0x40000028)
  }
  #[cfg(target_arch = "x86")]
  {
    Some(0x40000003)
  }
  #[cfg(not(any(
    target_arch = "x86_64",
    target_arch = "aarch64",
    target_arch = "riscv64",
    target_arch = "arm",
    target_arch = "x86"
  )))]
  {
    // Unknown architecture: skip arch validation.
    None
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::sandbox::{RendererLandlockPolicy, RendererSeccompPolicy};

  #[cfg(target_os = "linux")]
  fn get_rlimit(resource: libc::__rlimit_resource_t) -> (u64, u64) {
    let mut lim = libc::rlimit {
      rlim_cur: 0,
      rlim_max: 0,
    };
    let rc = unsafe { libc::getrlimit(resource, &mut lim) };
    assert_eq!(rc, 0, "getrlimit should succeed");
    (lim.rlim_cur as u64, lim.rlim_max as u64)
  }

  #[cfg(target_os = "linux")]
  #[test]
  fn configure_renderer_command_installs_sandbox() {
    const CHILD_ENV: &str = "FASTR_TEST_SANDBOX_CHILD";
    const EXPECT_AS_ENV: &str = "FASTR_TEST_SANDBOX_EXPECT_AS";
    const EXPECT_NOFILE_ENV: &str = "FASTR_TEST_SANDBOX_EXPECT_NOFILE";
    const EXPECT_NPROC_ENV: &str = "FASTR_TEST_SANDBOX_EXPECT_NPROC";

    let is_child = std::env::var_os(CHILD_ENV).is_some();
    if is_child {
      // Validate rlimits were applied.
      let expected_as: u64 = std::env::var(EXPECT_AS_ENV)
        .expect("expected as env")
        .parse()
        .expect("parse expected as");
      let expected_nofile: u64 = std::env::var(EXPECT_NOFILE_ENV)
        .expect("expected nofile env")
        .parse()
        .expect("parse expected nofile");
      let expected_nproc: u64 = std::env::var(EXPECT_NPROC_ENV)
        .expect("expected nproc env")
        .parse()
        .expect("parse expected nproc");

      let (cur_as, max_as) = get_rlimit(libc::RLIMIT_AS);
      assert_eq!(cur_as, expected_as, "RLIMIT_AS.cur should match");
      assert_eq!(max_as, expected_as, "RLIMIT_AS.max should match");

      let (cur_nofile, max_nofile) = get_rlimit(libc::RLIMIT_NOFILE);
      assert_eq!(
        cur_nofile, expected_nofile,
        "RLIMIT_NOFILE.cur should match"
      );
      assert_eq!(
        max_nofile, expected_nofile,
        "RLIMIT_NOFILE.max should match"
      );

      let (cur_core, max_core) = get_rlimit(libc::RLIMIT_CORE);
      assert_eq!(cur_core, 0, "core dumps should be disabled");
      assert_eq!(max_core, 0, "core dumps should be disabled");

      let (cur_nproc, max_nproc) = get_rlimit(libc::RLIMIT_NPROC);
      assert_eq!(cur_nproc, expected_nproc, "RLIMIT_NPROC.cur should match");
      assert_eq!(max_nproc, expected_nproc, "RLIMIT_NPROC.max should match");

      // Validate no_new_privs is set.
      let no_new_privs = unsafe { libc::prctl(libc::PR_GET_NO_NEW_PRIVS, 0, 0, 0, 0) };
      assert_eq!(no_new_privs, 1, "PR_GET_NO_NEW_PRIVS should report enabled");

      // Validate dumpable is disabled (no ptrace/coredumps).
      let dumpable = unsafe { libc::prctl(libc::PR_GET_DUMPABLE, 0, 0, 0, 0) };
      assert_eq!(dumpable, 0, "PR_GET_DUMPABLE should report disabled");

      // Validate PDEATHSIG is configured.
      let mut pdeathsig: libc::c_int = 0;
      let rc = unsafe { libc::prctl(libc::PR_GET_PDEATHSIG, &mut pdeathsig, 0, 0, 0) };
      assert_eq!(rc, 0, "PR_GET_PDEATHSIG should succeed");
      assert_eq!(
        pdeathsig,
        libc::SIGKILL,
        "expected PDEATHSIG to be configured to SIGKILL"
      );

      // Validate seccomp is active.
      let seccomp_mode = unsafe { libc::prctl(libc::PR_GET_SECCOMP, 0, 0, 0, 0) };
      assert_eq!(seccomp_mode, 2, "expected seccomp filter mode");

      // And that it blocks creating sockets.
      let fd = unsafe { libc::socket(libc::AF_INET, libc::SOCK_STREAM, 0) };
      assert_eq!(fd, -1, "socket() should be blocked by seccomp");
      let errno = std::io::Error::last_os_error()
        .raw_os_error()
        .expect("errno");
      assert_eq!(
        errno,
        libc::EPERM,
        "socket() should fail with EPERM under seccomp"
      );

      return;
    }

    // Parent: spawn a copy of the test binary and install the sandbox via `pre_exec`.
    let exe = std::env::current_exe().expect("current test exe path");

    // Pick limits that are guaranteed not to raise inherited limits.
    let (cur_as, max_as) = get_rlimit(libc::RLIMIT_AS);
    let desired_as = std::cmp::min(std::cmp::min(cur_as, max_as), 1024_u64 * 1024 * 1024);
    assert!(desired_as > 0, "expected a non-zero address-space max");

    let (cur_nofile, max_nofile) = get_rlimit(libc::RLIMIT_NOFILE);
    let desired_nofile = std::cmp::min(std::cmp::min(cur_nofile, max_nofile), 256_u64);
    assert!(desired_nofile > 0, "expected a non-zero nofile max");

    let (cur_nproc, max_nproc) = get_rlimit(libc::RLIMIT_NPROC);
    let desired_nproc = std::cmp::min(std::cmp::min(cur_nproc, max_nproc), 1024_u64);

    let config = RendererSandboxConfig {
      address_space_limit_bytes: Some(desired_as),
      nofile_limit: Some(desired_nofile),
      core_limit_bytes: Some(0),
      nproc_limit: Some(desired_nproc),
      landlock: RendererLandlockPolicy::Disabled,
      seccomp: RendererSeccompPolicy::RendererDefault,
      ..RendererSandboxConfig::default()
    };

    let test_name = "sandbox::spawn::tests::configure_renderer_command_installs_sandbox";
    let mut cmd = Command::new(exe);
    cmd
      .env(CHILD_ENV, "1")
      .env(EXPECT_AS_ENV, desired_as.to_string())
      .env(EXPECT_NOFILE_ENV, desired_nofile.to_string())
      .env(EXPECT_NPROC_ENV, desired_nproc.to_string())
      .arg("--exact")
      .arg(test_name)
      .arg("--nocapture");

    let mut cmd = configure_renderer_command(cmd, config).expect("configure sandbox");

    let output = cmd.output().expect("spawn sandboxed child test process");
    assert!(
      output.status.success(),
      "child process should exit successfully (stdout={}, stderr={})",
      String::from_utf8_lossy(&output.stdout),
      String::from_utf8_lossy(&output.stderr)
    );
  }
}
