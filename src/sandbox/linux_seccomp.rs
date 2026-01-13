//! Linux `seccomp-bpf` renderer sandbox.
//!
//! Implementation notes:
//! - Disables dumpability via `prctl(PR_SET_DUMPABLE, 0)` before installing seccomp to reduce
//!   `ptrace`/`/proc` information leaks and disable core dumps.
//! - Uses `PR_SET_NO_NEW_PRIVS` (mandatory for unprivileged seccomp).
//! - Installs a `SECCOMP_MODE_FILTER` program via the `seccomp()` syscall, attempting
//!   `SECCOMP_FILTER_FLAG_TSYNC` first (all threads) and falling back to `flags=0` on older kernels
//!   that reject TSYNC with `EINVAL`.
//! - The policy is a small denylist (returning `EPERM`) on top of a broad allowlist,
//!   with a conservative default action (`KILL_PROCESS`) for syscalls not explicitly allowed.

use super::{
  NetworkPolicy, RendererSandboxConfig, SandboxError, SandboxStatus, SeccompInstallRejectedReason,
  SeccompNotifSizes,
};
use std::io;

// Values from `linux/seccomp.h`.
const SECCOMP_SET_MODE_FILTER: u32 = 1;
const SECCOMP_GET_ACTION_AVAIL: u32 = 2;
const SECCOMP_GET_NOTIF_SIZES: u32 = 3;
const SECCOMP_FILTER_FLAG_TSYNC: u32 = 1;

pub(super) const SECCOMP_RET_KILL_PROCESS: u32 = 0x8000_0000;
const SECCOMP_RET_ALLOW: u32 = 0x7fff_0000;
pub(super) const SECCOMP_RET_ERRNO: u32 = 0x0005_0000;

// Values from `linux/filter.h`.
const BPF_LD: u16 = 0x00;
const BPF_W: u16 = 0x00;
const BPF_ABS: u16 = 0x20;
const BPF_JMP: u16 = 0x05;
const BPF_JEQ: u16 = 0x10;
const BPF_K: u16 = 0x00;
const BPF_RET: u16 = 0x06;

const BPF_LD_W_ABS: u16 = BPF_LD | BPF_W | BPF_ABS;
const BPF_JMP_JEQ_K: u16 = BPF_JMP | BPF_JEQ | BPF_K;
const BPF_RET_K: u16 = BPF_RET | BPF_K;

// Offsets into `struct seccomp_data` (from `linux/seccomp.h`).
const SECCOMP_DATA_NR_OFFSET: u32 = 0;
const SECCOMP_DATA_ARCH_OFFSET: u32 = 4;
// `args` starts immediately after the 64-bit instruction pointer.
// See `struct seccomp_data` in the kernel: `nr` (u32), `arch` (u32), `ip` (u64), `args[6]` (u64).
const SECCOMP_DATA_ARG0_OFFSET: u32 = 16;

// `AUDIT_ARCH_*` constants are defined in `linux/audit.h`. `libc` does not expose them
// consistently across targets, so we define the minimal set we need here.
//
// Values are `EM_* | __AUDIT_ARCH_{64BIT,LE}`.
#[cfg(target_arch = "x86_64")]
const AUDIT_ARCH: u32 = 0xC000_003E; // EM_X86_64 (62) | 64BIT | LE
#[cfg(target_arch = "aarch64")]
const AUDIT_ARCH: u32 = 0xC000_00B7; // EM_AARCH64 (183) | 64BIT | LE
#[cfg(target_arch = "x86")]
const AUDIT_ARCH: u32 = 0x4000_0003; // EM_386 (3) | LE
#[cfg(target_arch = "arm")]
const AUDIT_ARCH: u32 = 0x4000_0028; // EM_ARM (40) | LE
#[cfg(target_arch = "riscv64")]
const AUDIT_ARCH: u32 = 0xC000_00F3; // EM_RISCV (243) | 64BIT | LE

#[cfg(not(any(
  target_arch = "x86_64",
  target_arch = "aarch64",
  target_arch = "x86",
  target_arch = "arm",
  target_arch = "riscv64"
)))]
compile_error!("seccomp sandbox: unsupported Linux architecture (AUDIT_ARCH not defined)");

#[inline]
const fn bpf_stmt(code: u16, k: u32) -> libc::sock_filter {
  libc::sock_filter {
    code,
    jt: 0,
    jf: 0,
    k,
  }
}

#[inline]
const fn bpf_jump(code: u16, k: u32, jt: u8, jf: u8) -> libc::sock_filter {
  libc::sock_filter { code, jt, jf, k }
}

fn build_renderer_filter(config: RendererSandboxConfig) -> Vec<libc::sock_filter> {
  let mut filter = Vec::<libc::sock_filter>::new();

  // Validate architecture early. If this doesn't match, the syscall numbers below are wrong.
  filter.push(bpf_stmt(BPF_LD_W_ABS, SECCOMP_DATA_ARCH_OFFSET));
  // If arch == AUDIT_ARCH: skip the kill, otherwise KILL_PROCESS.
  filter.push(bpf_jump(BPF_JMP_JEQ_K, AUDIT_ARCH, 1, 0));
  filter.push(bpf_stmt(BPF_RET_K, SECCOMP_RET_KILL_PROCESS));

  // Load syscall number into the accumulator.
  filter.push(bpf_stmt(BPF_LD_W_ABS, SECCOMP_DATA_NR_OFFSET));

  // Socket policy must run before the generic denylist/allowlist rules because it inspects syscall
  // args (`domain`).
  let ret_eperm = SECCOMP_RET_ERRNO | (libc::EPERM as u32);
  match config.network_policy {
    NetworkPolicy::DenyAllSockets => {
      // Deny all socket creation, including AF_UNIX. This is the default conservative policy.
      filter.push(bpf_jump(BPF_JMP_JEQ_K, libc::SYS_socket as u32, 0, 1));
      filter.push(bpf_stmt(BPF_RET_K, ret_eperm));
      filter.push(bpf_jump(BPF_JMP_JEQ_K, libc::SYS_socketpair as u32, 0, 1));
      filter.push(bpf_stmt(BPF_RET_K, ret_eperm));
    }
    NetworkPolicy::AllowUnixSocketsOnly => {
      // Allow `socket(AF_UNIX, ...)` while denying AF_INET/AF_INET6/etc with EPERM.
      filter.push(bpf_jump(BPF_JMP_JEQ_K, libc::SYS_socket as u32, 0, 4));
      // args[0] = `domain` (lower 32-bits).
      filter.push(bpf_stmt(BPF_LD_W_ABS, SECCOMP_DATA_ARG0_OFFSET));
      // If domain == AF_UNIX: allow; otherwise: EPERM.
      filter.push(bpf_jump(BPF_JMP_JEQ_K, libc::AF_UNIX as u32, 1, 0));
      filter.push(bpf_stmt(BPF_RET_K, ret_eperm));
      filter.push(bpf_stmt(BPF_RET_K, SECCOMP_RET_ALLOW));

      // Allow `socketpair(AF_UNIX, ...)` while denying all other families with EPERM.
      filter.push(bpf_jump(
        BPF_JMP_JEQ_K,
        libc::SYS_socketpair as u32,
        0,
        4,
      ));
      filter.push(bpf_stmt(BPF_LD_W_ABS, SECCOMP_DATA_ARG0_OFFSET));
      filter.push(bpf_jump(BPF_JMP_JEQ_K, libc::AF_UNIX as u32, 1, 0));
      filter.push(bpf_stmt(BPF_RET_K, ret_eperm));
      filter.push(bpf_stmt(BPF_RET_K, SECCOMP_RET_ALLOW));
    }
  }

  // Explicit denylist: return EPERM (tests assert this) rather than killing the process.
  //
  // Keep this list conservative and focused on high-level capability denial for the renderer:
  // filesystem path operations, networking, process execution, and obvious kernel escape surfaces.
  let mut deny = Vec::<libc::c_long>::new();
  deny.extend_from_slice(&[
    // Filesystem opens.
    libc::SYS_open,
    libc::SYS_openat,
    libc::SYS_openat2,
    libc::SYS_creat,
    libc::SYS_open_by_handle_at,
    libc::SYS_name_to_handle_at,
    // Filesystem metadata / enumeration (defense in depth).
    //
    // These syscalls don't necessarily grant file contents, but they can leak information about the
    // host filesystem (existence, ownership, timestamps, directory structure). Returning EPERM is
    // also more ergonomic than the default KILL policy for unexpected libc behavior.
    libc::SYS_statx,
    libc::SYS_newfstatat,
    libc::SYS_access,
    libc::SYS_faccessat,
    // `faccessat2` is a newer variant used by modern glibc; deny it explicitly when available so
    // filesystem probing fails with EPERM instead of hitting the default KILL action.
    #[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
    libc::SYS_faccessat2,
    libc::SYS_getdents64,
    // 32-bit ABIs may use `getdents` instead of `getdents64`.
    #[cfg(any(target_arch = "x86", target_arch = "arm"))]
    libc::SYS_getdents,
    libc::SYS_readlink,
    libc::SYS_readlinkat,
    libc::SYS_statfs,
    // Filesystem mutation.
    libc::SYS_unlink,
    libc::SYS_unlinkat,
    libc::SYS_rename,
    libc::SYS_renameat,
    libc::SYS_renameat2,
    libc::SYS_mkdir,
    libc::SYS_mkdirat,
    libc::SYS_rmdir,
    libc::SYS_link,
    libc::SYS_linkat,
    libc::SYS_symlink,
    libc::SYS_symlinkat,
    libc::SYS_mknod,
    libc::SYS_mknodat,
    libc::SYS_chmod,
    libc::SYS_fchmod,
    libc::SYS_fchmodat,
    libc::SYS_chown,
    libc::SYS_fchown,
    libc::SYS_fchownat,
    libc::SYS_utime,
    libc::SYS_utimes,
    libc::SYS_utimensat,
    libc::SYS_truncate,
    libc::SYS_ftruncate,
    // Mounting / namespace escape.
    libc::SYS_mount,
    libc::SYS_umount2,
    libc::SYS_pivot_root,
    libc::SYS_chroot,
    // Process execution.
    libc::SYS_execve,
    libc::SYS_execveat,
    // Introspection / escape.
    libc::SYS_ptrace,
    libc::SYS_bpf,
    libc::SYS_perf_event_open,
    libc::SYS_kexec_load,
    libc::SYS_reboot,
    libc::SYS_swapon,
    libc::SYS_swapoff,
    libc::SYS_init_module,
    libc::SYS_finit_module,
    libc::SYS_delete_module,
    // High-risk kernel attack surface that should never be needed in a renderer process.
    libc::SYS_io_uring_setup,
    libc::SYS_io_uring_enter,
    libc::SYS_io_uring_register,
    libc::SYS_process_vm_readv,
    libc::SYS_process_vm_writev,
    libc::SYS_kcmp,
    libc::SYS_userfaultfd,
    libc::SYS_keyctl,
    libc::SYS_add_key,
    libc::SYS_request_key,
    // Privilege/namespace syscalls (even if they'd fail, make it explicit).
    libc::SYS_unshare,
    libc::SYS_setns,
  ]);

  // pidfd-based process and FD introspection.
  #[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
  deny.extend_from_slice(&[
    libc::SYS_pidfd_open,
    libc::SYS_pidfd_getfd,
    libc::SYS_pidfd_send_signal,
  ]);

  // Signal-queue syscalls can deliver `siginfo` payloads to other processes/threads. These are not
  // needed by the renderer sandbox and widen the cross-process control surface.
  deny.extend_from_slice(&[libc::SYS_rt_sigqueueinfo, libc::SYS_rt_tgsigqueueinfo]);

  // Filesystem notification (expand kernel surface; should never be needed in a renderer).
  #[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
  deny.extend_from_slice(&[libc::SYS_fanotify_init, libc::SYS_fanotify_mark]);

  // 32-bit x86 uses the `socketcall(2)` multiplexer for socket operations. Seccomp cannot safely
  // restrict socket families because the arguments live behind a user-space pointer, so we deny the
  // syscall entirely for security.
  #[cfg(target_arch = "x86")]
  deny.push(libc::SYS_socketcall);

  // Socket operations are denied by default, but can be allowed for Unix-domain IPC.
  //
  // Seccomp filters cannot inspect `sockaddr` pointers, so the security model for
  // `AllowUnixSocketsOnly` relies on denying creation of non-Unix sockets (see the `socket(2)` /
  // `socketpair(2)` rules above) and sanitising inherited file descriptors.
  if matches!(config.network_policy, NetworkPolicy::DenyAllSockets) {
    deny.extend_from_slice(&[
      libc::SYS_connect,
      libc::SYS_bind,
      libc::SYS_listen,
      libc::SYS_accept,
      libc::SYS_accept4,
      libc::SYS_sendto,
      libc::SYS_sendmsg,
      libc::SYS_recvfrom,
      libc::SYS_recvmsg,
      libc::SYS_setsockopt,
      libc::SYS_getsockopt,
      libc::SYS_getsockname,
      libc::SYS_getpeername,
      libc::SYS_shutdown,
    ]);
  }

  for nr in deny {
    filter.push(bpf_jump(BPF_JMP_JEQ_K, nr as u32, 0, 1));
    filter.push(bpf_stmt(BPF_RET_K, ret_eperm));
  }

  // Allowlist: syscalls expected to be used by a typical renderer process at runtime.
  //
  // Note: this list intentionally includes common thread/allocator/time/file-descriptor syscalls
  // so the process can continue to run after the sandbox is installed.
  let mut allow = Vec::<libc::c_long>::new();
  allow.extend_from_slice(&[
    // Process/thread lifecycle.
    libc::SYS_exit,
    libc::SYS_exit_group,
    libc::SYS_clone,
    libc::SYS_clone3,
    libc::SYS_fork,
    libc::SYS_vfork,
    libc::SYS_wait4,
    libc::SYS_waitid,
    libc::SYS_getpid,
    libc::SYS_getppid,
    libc::SYS_gettid,
    libc::SYS_getuid,
    libc::SYS_geteuid,
    libc::SYS_getgid,
    libc::SYS_getegid,
    libc::SYS_getresuid,
    libc::SYS_getresgid,
    libc::SYS_getgroups,
    libc::SYS_set_tid_address,
    libc::SYS_set_robust_list,
    libc::SYS_tgkill,
    libc::SYS_kill,
    libc::SYS_prctl,
    libc::SYS_prlimit64,
    libc::SYS_getrlimit,
    libc::SYS_setrlimit,
    libc::SYS_setpgid,
    libc::SYS_getpgid,
    libc::SYS_getpgrp,
    libc::SYS_setsid,
    libc::SYS_getsid,
    // Signals.
    libc::SYS_rt_sigaction,
    libc::SYS_rt_sigprocmask,
    libc::SYS_rt_sigreturn,
    libc::SYS_sigaltstack,
    // Memory management.
    libc::SYS_brk,
    libc::SYS_mmap,
    libc::SYS_munmap,
    libc::SYS_mprotect,
    libc::SYS_mremap,
    libc::SYS_madvise,
    // Randomness and time.
    libc::SYS_getrandom,
    libc::SYS_clock_gettime,
    libc::SYS_clock_getres,
    libc::SYS_clock_nanosleep,
    libc::SYS_nanosleep,
    libc::SYS_sched_yield,
    // File descriptor I/O (on already-open fds, pipes, etc.).
    libc::SYS_read,
    libc::SYS_write,
    libc::SYS_readv,
    libc::SYS_writev,
    libc::SYS_pread64,
    libc::SYS_pwrite64,
    libc::SYS_close,
    libc::SYS_close_range,
    libc::SYS_lseek,
    libc::SYS_fcntl,
    libc::SYS_ioctl,
    libc::SYS_dup,
    libc::SYS_dup2,
    libc::SYS_dup3,
    libc::SYS_pipe,
    libc::SYS_pipe2,
    libc::SYS_poll,
    libc::SYS_ppoll,
    libc::SYS_select,
    libc::SYS_pselect6,
    libc::SYS_epoll_create1,
    libc::SYS_epoll_ctl,
    libc::SYS_epoll_wait,
    libc::SYS_epoll_pwait,
    libc::SYS_eventfd2,
    libc::SYS_signalfd4,
    // Misc.
    libc::SYS_uname,
    libc::SYS_sysinfo,
    libc::SYS_gettimeofday,
    libc::SYS_getrusage,
    libc::SYS_getcwd,
    libc::SYS_chdir,
    libc::SYS_fchdir,
    libc::SYS_umask,
    libc::SYS_futex,
    libc::SYS_restart_syscall,
    // Threading/runtime helpers that glibc/Rust may use after the sandbox is installed.
    //
    // - `rseq` is used by newer glibc for per-thread fastpaths (optional but may be attempted).
    // - `membarrier` can be used as part of rseq/tls/runtime mechanisms.
    // - `sched_getaffinity` is commonly used by thread pools to size themselves.
    libc::SYS_rseq,
    libc::SYS_membarrier,
    libc::SYS_sched_getaffinity,
    libc::SYS_getcpu,
    // Allow querying metadata about existing fds.
    libc::SYS_fstat,
    libc::SYS_fstatfs,
  ]);

  if matches!(config.network_policy, NetworkPolicy::AllowUnixSocketsOnly) {
    allow.extend_from_slice(&[
      libc::SYS_connect,
      libc::SYS_bind,
      libc::SYS_listen,
      libc::SYS_accept,
      libc::SYS_accept4,
      libc::SYS_sendto,
      libc::SYS_sendmsg,
      libc::SYS_recvfrom,
      libc::SYS_recvmsg,
      libc::SYS_setsockopt,
      libc::SYS_getsockopt,
      libc::SYS_getsockname,
      libc::SYS_getpeername,
      libc::SYS_shutdown,
    ]);
  }

  #[cfg(target_arch = "x86_64")]
  allow.push(libc::SYS_arch_prctl);
  #[cfg(target_arch = "x86")]
  allow.extend_from_slice(&[libc::SYS_set_thread_area, libc::SYS_get_thread_area]);

  for nr in allow {
    filter.push(bpf_jump(BPF_JMP_JEQ_K, nr as u32, 0, 1));
    filter.push(bpf_stmt(BPF_RET_K, SECCOMP_RET_ALLOW));
  }

  // Default: kill the process if an unexpected syscall is hit. This is intentionally strict;
  // relax the allowlist as needed as the renderer architecture evolves.
  filter.push(bpf_stmt(BPF_RET_K, SECCOMP_RET_KILL_PROCESS));

  filter
}

pub(super) fn apply_renderer_sandbox_prelude_linux() -> Result<(), SandboxError> {
  super::linux_set_parent_death_signal().map_err(|source| SandboxError::SetParentDeathSignalFailed {
    source,
  })?;

  // Disable dumpability so same-UID processes cannot `ptrace` attach and sensitive `/proc/<pid>`
  // entries are protected. This also disables core dumps.
  //
  // SAFETY: `prctl` is a process-global syscall. We pass the documented arguments for
  // `PR_SET_DUMPABLE`.
  let rc = unsafe { libc::prctl(libc::PR_SET_DUMPABLE, 0, 0, 0, 0) };
  if rc != 0 {
    return Err(SandboxError::SetDumpableFailed {
      source: io::Error::last_os_error(),
    });
  }
  Ok(())
}

pub(super) fn apply_renderer_sandbox_linux(
  config: RendererSandboxConfig,
) -> Result<SandboxStatus, SandboxError> {
  apply_renderer_sandbox_prelude_linux()?;

  // 1) no_new_privs
  // SAFETY: `prctl` is a process-scoped syscall. We pass the documented arguments.
  let rc = unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) };
  if rc != 0 {
    return Err(SandboxError::EnableNoNewPrivsFailed {
      source: io::Error::last_os_error(),
    });
  }

  // 2) Build and install the seccomp filter.
  let mut filter = build_renderer_filter(config);
  let len: u16 = filter
    .len()
    .try_into()
    .map_err(|_| SandboxError::SeccompFilterTooLong { len: filter.len() })?;
  let prog = libc::sock_fprog {
    len,
    filter: filter.as_mut_ptr(),
  };

  // SAFETY: `seccomp` syscall expects a pointer to a valid `sock_fprog` which contains a valid
  // pointer/length pair for the filter program. The kernel copies the filter; the pointer does
  // not need to outlive the syscall.
  let mut do_install = |flags: u32| seccomp_set_mode_filter(flags, &prog);
  let status = install_filter_with_tsync_fallback(config, &mut do_install)
    .map_err(|source| seccomp_error_from_io(source))?;
  Ok(status)
}

fn seccomp_error_from_io(source: io::Error) -> SandboxError {
  let errno = source.raw_os_error().unwrap_or_default();
  if errno == libc::EPERM {
    return SandboxError::SeccompInstallRejected {
      reason: SeccompInstallRejectedReason::PermissionDenied,
      errno,
      source,
    };
  }
  if errno == libc::EINVAL {
    return SandboxError::SeccompInstallRejected {
      reason: SeccompInstallRejectedReason::InvalidArgument,
      errno,
      source,
    };
  }
  SandboxError::SeccompInstallFailed { errno, source }
}

fn install_filter_with_tsync_fallback<F>(
  config: RendererSandboxConfig,
  do_install: &mut F,
) -> io::Result<SandboxStatus>
where
  F: FnMut(u32) -> io::Result<()>,
{
  if config.force_disable_tsync {
    do_install(0)?;
    return Ok(SandboxStatus::AppliedWithoutTsync);
  }

  match do_install(SECCOMP_FILTER_FLAG_TSYNC) {
    Ok(()) => Ok(SandboxStatus::Applied),
    Err(err) => {
      if err.raw_os_error() == Some(libc::EINVAL) {
        // TSYNC is not supported by the running kernel; retry without flags.
        do_install(0)?;
        Ok(SandboxStatus::AppliedWithoutTsync)
      } else {
        Err(err)
      }
    }
  }
}

fn seccomp_set_mode_filter(flags: u32, prog: &libc::sock_fprog) -> io::Result<()> {
  // SAFETY: `seccomp` is a process-global syscall. We pass a valid pointer to a `sock_fprog`.
  // The kernel copies the filter program and treats the user memory as read-only.
  let rc = unsafe {
    libc::syscall(
      libc::SYS_seccomp,
      SECCOMP_SET_MODE_FILTER,
      flags,
      prog as *const libc::sock_fprog,
    )
  };
  if rc < 0 {
    return Err(io::Error::last_os_error());
  }
  Ok(())
}

#[cfg(test)]
mod tests {
  use super::*;

  fn err(errno: i32) -> io::Error {
    io::Error::from_raw_os_error(errno)
  }

  #[test]
  fn tsync_retry_on_einval() {
    let config = RendererSandboxConfig::default();
    let mut flags_seen = Vec::new();
    let mut calls = 0usize;
    let mut install = |flags: u32| {
      flags_seen.push(flags);
      calls += 1;
      if calls == 1 {
        return Err(err(libc::EINVAL));
      }
      Ok(())
    };
    let status =
      install_filter_with_tsync_fallback(config, &mut install).expect("expected fallback to work");
    assert_eq!(status, SandboxStatus::AppliedWithoutTsync);
    assert_eq!(flags_seen, vec![SECCOMP_FILTER_FLAG_TSYNC, 0]);
  }

  #[test]
  fn tsync_no_retry_on_other_error() {
    let config = RendererSandboxConfig::default();
    let mut calls = 0usize;
    let mut install = |_flags: u32| {
      calls += 1;
      Err(err(libc::EPERM))
    };
    let result = install_filter_with_tsync_fallback(config, &mut install);
    assert!(
      matches!(result, Err(ref e) if e.raw_os_error() == Some(libc::EPERM)),
      "expected EPERM error"
    );
    assert_eq!(calls, 1, "expected no retry for non-EINVAL errors");
  }

  #[test]
  fn force_disable_tsync_skips_first_attempt() {
    let config = RendererSandboxConfig {
      force_disable_tsync: true,
      ..Default::default()
    };
    let mut flags_seen = Vec::new();
    let mut install = |flags: u32| {
      flags_seen.push(flags);
      Ok(())
    };
    let status = install_filter_with_tsync_fallback(config, &mut install)
      .expect("expected install to succeed");
    assert_eq!(status, SandboxStatus::AppliedWithoutTsync);
    assert_eq!(flags_seen, vec![0]);
  }
}

// --- Preflight helpers ------------------------------------------------------------------------

pub(super) fn prctl_get_seccomp_mode() -> io::Result<i32> {
  // SAFETY: `prctl` is a process-global syscall. The PR_GET_SECCOMP operation ignores the remaining
  // arguments.
  let rc = unsafe { libc::prctl(libc::PR_GET_SECCOMP, 0, 0, 0, 0) };
  if rc == -1 {
    return Err(io::Error::last_os_error());
  }
  Ok(rc)
}

pub(super) fn prctl_get_no_new_privs() -> io::Result<bool> {
  // SAFETY: `prctl` is a process-global syscall. The PR_GET_NO_NEW_PRIVS operation ignores the
  // remaining arguments.
  let rc = unsafe { libc::prctl(libc::PR_GET_NO_NEW_PRIVS, 0, 0, 0, 0) };
  if rc == -1 {
    return Err(io::Error::last_os_error());
  }
  Ok(rc != 0)
}

pub(super) fn seccomp_action_avail(action: u32) -> io::Result<()> {
  let mut requested_action = action;
  // SAFETY: `requested_action` is a writable u32 as required by the kernel ABI.
  let rc = unsafe {
    seccomp_syscall(
      SECCOMP_GET_ACTION_AVAIL,
      0,
      std::ptr::addr_of_mut!(requested_action).cast(),
    )
  };
  if rc == -1 {
    return Err(io::Error::last_os_error());
  }
  Ok(())
}

pub(super) fn seccomp_get_notif_sizes() -> io::Result<SeccompNotifSizes> {
  let mut sizes = SeccompNotifSizes {
    seccomp_notif: 0,
    seccomp_notif_resp: 0,
    seccomp_data: 0,
  };
  // SAFETY: `sizes` is a writable struct matching the kernel ABI for `SECCOMP_GET_NOTIF_SIZES`.
  let rc = unsafe {
    seccomp_syscall(
      SECCOMP_GET_NOTIF_SIZES,
      0,
      std::ptr::addr_of_mut!(sizes).cast(),
    )
  };
  if rc == -1 {
    return Err(io::Error::last_os_error());
  }
  Ok(sizes)
}

unsafe fn seccomp_syscall(op: u32, flags: u32, args: *mut libc::c_void) -> libc::c_long {
  libc::syscall(
    libc::SYS_seccomp,
    libc::c_ulong::from(op),
    libc::c_ulong::from(flags),
    args,
  )
}
