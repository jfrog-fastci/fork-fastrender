//! Linux `seccomp-bpf` renderer sandbox.
//!
//! Implementation notes:
//! - Uses `PR_SET_NO_NEW_PRIVS` (mandatory for unprivileged seccomp).
//! - Installs a `SECCOMP_MODE_FILTER` program via the `seccomp()` syscall with
//!   `SECCOMP_FILTER_FLAG_TSYNC` so the policy applies to all threads.
//! - The policy is a small denylist (returning `EPERM`) on top of a broad allowlist,
//!   with a conservative default action (`KILL_PROCESS`) for syscalls not explicitly allowed.

use super::{RendererSandboxConfig, SandboxError, SandboxStatus, SeccompInstallRejectedReason};
use std::io;

// Values from `linux/seccomp.h`.
const SECCOMP_SET_MODE_FILTER: u32 = 1;
const SECCOMP_FILTER_FLAG_TSYNC: u32 = 1;

const SECCOMP_RET_KILL_PROCESS: u32 = 0x8000_0000;
const SECCOMP_RET_ALLOW: u32 = 0x7fff_0000;
const SECCOMP_RET_ERRNO: u32 = 0x0005_0000;

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

fn build_renderer_filter() -> Vec<libc::sock_filter> {
  let mut filter = Vec::<libc::sock_filter>::new();

  // Validate architecture early. If this doesn't match, the syscall numbers below are wrong.
  filter.push(bpf_stmt(BPF_LD_W_ABS, SECCOMP_DATA_ARCH_OFFSET));
  // If arch == AUDIT_ARCH: skip the kill, otherwise KILL_PROCESS.
  filter.push(bpf_jump(BPF_JMP_JEQ_K, AUDIT_ARCH, 1, 0));
  filter.push(bpf_stmt(BPF_RET_K, SECCOMP_RET_KILL_PROCESS));

  // Load syscall number into the accumulator.
  filter.push(bpf_stmt(BPF_LD_W_ABS, SECCOMP_DATA_NR_OFFSET));

  // `socket(2)` is the main surface for network access, but renderer IPC may need Unix-domain
  // sockets. Allow `socket(AF_UNIX, ...)` while denying AF_INET/AF_INET6/etc with EPERM.
  //
  // This must run before the generic denylist/allowlist rules because it inspects syscall args.
  filter.push(bpf_jump(BPF_JMP_JEQ_K, libc::SYS_socket as u32, 0, 4));
  // args[0] = `domain` (lower 32-bits).
  filter.push(bpf_stmt(BPF_LD_W_ABS, SECCOMP_DATA_ARG0_OFFSET));
  // If domain == AF_UNIX: allow; otherwise: EPERM.
  filter.push(bpf_jump(BPF_JMP_JEQ_K, libc::AF_UNIX as u32, 1, 0));
  filter.push(bpf_stmt(
    BPF_RET_K,
    SECCOMP_RET_ERRNO | (libc::EPERM as u32),
  ));
  filter.push(bpf_stmt(BPF_RET_K, SECCOMP_RET_ALLOW));

  // Explicit denylist: return EPERM (tests assert this) rather than killing the process.
  let deny = [
    // Filesystem access.
    libc::SYS_open,
    libc::SYS_openat,
    libc::SYS_openat2,
    libc::SYS_creat,
    // Defense in depth: process execution.
    libc::SYS_execve,
    libc::SYS_execveat,
    // Network / sockets.
    libc::SYS_connect,
    libc::SYS_bind,
    libc::SYS_listen,
    libc::SYS_accept,
    libc::SYS_accept4,
    // High-risk kernel attack surface that should never be needed in a renderer process.
    libc::SYS_bpf,
    libc::SYS_perf_event_open,
    libc::SYS_ptrace,
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
  ];
  for nr in deny {
    filter.push(bpf_jump(BPF_JMP_JEQ_K, nr as u32, 0, 1));
    filter.push(bpf_stmt(
      BPF_RET_K,
      SECCOMP_RET_ERRNO | (libc::EPERM as u32),
    ));
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
    // Local IPC primitives (not network).
    libc::SYS_socketpair,
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
    // Allow querying metadata about existing fds.
    libc::SYS_fstat,
    libc::SYS_fstatfs,
  ]);

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

pub(super) fn apply_renderer_sandbox_linux(
  _config: RendererSandboxConfig,
) -> Result<SandboxStatus, SandboxError> {
  // 1) no_new_privs
  // SAFETY: `prctl` is a process-scoped syscall. We pass the documented arguments.
  let rc = unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) };
  if rc != 0 {
    return Err(SandboxError::EnableNoNewPrivsFailed {
      source: io::Error::last_os_error(),
    });
  }

  // 2) Build and install the seccomp filter.
  let mut filter = build_renderer_filter();
  let prog = libc::sock_fprog {
    len: filter
      .len()
      .try_into()
      .expect("seccomp filter length should fit in u16"),
    filter: filter.as_mut_ptr(),
  };

  // SAFETY: `seccomp` syscall expects a pointer to a valid `sock_fprog` which contains a valid
  // pointer/length pair for the filter program. The kernel copies the filter; the pointer does
  // not need to outlive the syscall.
  let rc = unsafe {
    libc::syscall(
      libc::SYS_seccomp,
      SECCOMP_SET_MODE_FILTER,
      SECCOMP_FILTER_FLAG_TSYNC,
      &prog as *const libc::sock_fprog,
    )
  };
  if rc != 0 {
    let source = io::Error::last_os_error();
    let errno = source.raw_os_error().unwrap_or_default();
    if errno == libc::EPERM {
      return Err(SandboxError::SeccompInstallRejected {
        reason: SeccompInstallRejectedReason::PermissionDenied,
        errno,
        source,
      });
    }
    if errno == libc::EINVAL {
      return Err(SandboxError::SeccompInstallRejected {
        reason: SeccompInstallRejectedReason::InvalidArgument,
        errno,
        source,
      });
    }
    return Err(SandboxError::SeccompInstallFailed { errno, source });
  }

  Ok(SandboxStatus::Applied)
}
