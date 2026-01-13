//! Helper for spawning a renderer subprocess with sandboxing configured.
//!
//! The goal is to minimize the "unsandboxed window" by installing security
//! restrictions immediately after `fork(2)` and before `execve(2)`.

use crate::sandbox::{RendererSandboxConfig, RendererSandboxError};
use std::process::Command;

#[cfg(all(unix, target_os = "linux"))]
use std::os::unix::process::CommandExt;

/// Configure `cmd` so the spawned renderer process is sandboxed as early as possible.
///
/// On Linux this uses `CommandExt::pre_exec` to run the sandbox setup in the child
/// process right after `fork` and right before `exec`.
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
pub fn configure_renderer_command(
  cmd: &mut Command,
  config: RendererSandboxConfig,
) -> Result<(), RendererSandboxError> {
  #[cfg(all(unix, target_os = "linux"))]
  {
    let cfg = LinuxPreExecConfig::try_from_config(config)?;

    // SAFETY: `pre_exec` is unsafe because the closure runs after fork. The
    // closure uses only async-signal-safe syscalls and does not allocate.
    unsafe {
      cmd.pre_exec(move || linux_pre_exec(cfg));
    }
    return Ok(());
  }

  #[cfg(not(all(unix, target_os = "linux")))]
  {
    let _ = (cmd, config);
    return Ok(());
  }
}

#[cfg(all(unix, target_os = "linux"))]
#[derive(Debug, Clone, Copy)]
struct LinuxPreExecConfig {
  rlimit_as: Option<libc::rlim_t>,
  rlimit_nofile: Option<libc::rlim_t>,
  rlimit_core: Option<libc::rlim_t>,
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
  // 1) Apply rlimits.
  if let Some(limit) = cfg.rlimit_as {
    apply_rlimit_hard_ceiling(libc::RLIMIT_AS, limit)?;
  }
  if let Some(limit) = cfg.rlimit_nofile {
    apply_rlimit_hard_ceiling(libc::RLIMIT_NOFILE, limit)?;
  }
  if let Some(limit) = cfg.rlimit_core {
    apply_rlimit_hard_ceiling(libc::RLIMIT_CORE, limit)?;
  }

  // 2) no_new_privs is required before installing seccomp/landlock without caps.
  set_no_new_privs()?;

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

  // Never attempt to raise limits inside the sandbox.
  let effective = if requested > current.rlim_max {
    current.rlim_max
  } else {
    requested
  };

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

// --- Landlock (best-effort) --------------------------------------------------

#[cfg(all(unix, target_os = "linux"))]
#[repr(C)]
struct LandlockRulesetAttrV1 {
  handled_access_fs: u64,
}

#[cfg(all(unix, target_os = "linux"))]
fn apply_landlock_restrict_writes_best_effort() -> std::io::Result<()> {
  // Landlock is optional; if the kernel doesn't support it (ENOSYS / EOPNOTSUPP),
  // we treat it as a no-op.

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
      libc::SYS_landlock_create_ruleset,
      &attr as *const LandlockRulesetAttrV1,
      std::mem::size_of::<LandlockRulesetAttrV1>(),
      0,
    )
  } as libc::c_int;

  if fd < 0 {
    let err = std::io::Error::last_os_error();
    match err.raw_os_error() {
      Some(libc::ENOSYS) | Some(libc::EOPNOTSUPP) => return Ok(()),
      _ => return Err(err),
    }
  }

  let rc = unsafe { libc::syscall(libc::SYS_landlock_restrict_self, fd, 0) };
  if rc != 0 {
    let err = std::io::Error::last_os_error();
    unsafe {
      libc::close(fd);
    }
    match err.raw_os_error() {
      Some(libc::ENOSYS) | Some(libc::EOPNOTSUPP) => return Ok(()),
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
    libc::sock_filter { code, jt: 0, jf: 0, k }
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

      let (cur_as, max_as) = get_rlimit(libc::RLIMIT_AS);
      assert_eq!(cur_as, expected_as, "RLIMIT_AS.cur should match");
      assert_eq!(max_as, expected_as, "RLIMIT_AS.max should match");

      let (cur_nofile, max_nofile) = get_rlimit(libc::RLIMIT_NOFILE);
      assert_eq!(cur_nofile, expected_nofile, "RLIMIT_NOFILE.cur should match");
      assert_eq!(max_nofile, expected_nofile, "RLIMIT_NOFILE.max should match");

      let (cur_core, max_core) = get_rlimit(libc::RLIMIT_CORE);
      assert_eq!(cur_core, 0, "core dumps should be disabled");
      assert_eq!(max_core, 0, "core dumps should be disabled");

      // Validate no_new_privs is set.
      let no_new_privs = unsafe { libc::prctl(libc::PR_GET_NO_NEW_PRIVS, 0, 0, 0, 0) };
      assert_eq!(no_new_privs, 1, "PR_GET_NO_NEW_PRIVS should report enabled");

      // Validate seccomp is active.
      let seccomp_mode = unsafe { libc::prctl(libc::PR_GET_SECCOMP, 0, 0, 0, 0) };
      assert_eq!(seccomp_mode, 2, "expected seccomp filter mode");

      // And that it blocks creating sockets.
      let fd = unsafe { libc::socket(libc::AF_INET, libc::SOCK_STREAM, 0) };
      assert_eq!(fd, -1, "socket() should be blocked by seccomp");
      let errno = std::io::Error::last_os_error()
        .raw_os_error()
        .expect("errno");
      assert_eq!(errno, libc::EPERM, "socket() should fail with EPERM under seccomp");

      return;
    }

    // Parent: spawn a copy of the test binary and install the sandbox via `pre_exec`.
    let exe = std::env::current_exe().expect("current test exe path");

    // Pick limits that are guaranteed not to raise the inherited hard maximum.
    let (_cur_as, max_as) = get_rlimit(libc::RLIMIT_AS);
    let desired_as = std::cmp::min(max_as, 1024_u64 * 1024 * 1024);
    assert!(desired_as > 0, "expected a non-zero address-space max");

    let (_cur_nofile, max_nofile) = get_rlimit(libc::RLIMIT_NOFILE);
    let desired_nofile = std::cmp::min(max_nofile, 256_u64);
    assert!(desired_nofile > 0, "expected a non-zero nofile max");

    let config = RendererSandboxConfig {
      address_space_limit_bytes: Some(desired_as),
      nofile_limit: Some(desired_nofile),
      core_limit_bytes: Some(0),
      landlock: RendererLandlockPolicy::Disabled,
      seccomp: RendererSeccompPolicy::RendererDefault,
      ..RendererSandboxConfig::default()
    };

    let test_name = "sandbox::spawn::tests::configure_renderer_command_installs_sandbox";
    let mut cmd = Command::new(exe);
    cmd.env(CHILD_ENV, "1")
      .env(EXPECT_AS_ENV, desired_as.to_string())
      .env(EXPECT_NOFILE_ENV, desired_nofile.to_string())
      .arg("--exact")
      .arg(test_name)
      .arg("--nocapture");

    configure_renderer_command(&mut cmd, config).expect("configure sandbox");

    let output = cmd.output().expect("spawn sandboxed child test process");
    assert!(
      output.status.success(),
      "child process should exit successfully (stdout={}, stderr={})",
      String::from_utf8_lossy(&output.stdout),
      String::from_utf8_lossy(&output.stderr)
    );
  }
}
