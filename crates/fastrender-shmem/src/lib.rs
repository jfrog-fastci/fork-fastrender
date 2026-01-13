//! Shared memory regions for FastRender's multiprocess architecture (browser ↔ renderer).
//!
//! FastRender's multiprocess architecture needs a fast way to share large pixel buffers across
//! processes. Shared memory is the usual building block: the renderer writes RGBA frames into a
//! region, and the browser reads and composites them.
//!
//! ## Backends and sandboxing
//!
//! A common POSIX approach is to use `shm_open` with a random global name (typically in `/dev/shm`)
//! and send that name to the other process. This works, but it has undesirable properties for
//! sandboxed renderers:
//!
//! - It relies on a *global namespace*. A compromised renderer could attempt to probe or create
//!   other shared-memory objects if `shm_open` is permitted.
//! - Sandboxes (seccomp/landlock/namespaces) often want to deny `shm_open` or restrict access to
//!   global namespaces entirely.
//!
//! On Linux we support a backend based on `memfd_create` (with an `O_TMPFILE` fallback), which
//! produces an anonymous file descriptor that has **no global name**. The browser can create the
//! shared memory and pass the FD to the renderer using FD inheritance (or future SCM_RIGHTS
//! passing).
//!
//! ## Linux memfd sealing policy
//!
//! When a shared-memory file descriptor crosses an IPC boundary it must be treated as untrusted.
//! In particular, if the receiver `mmap`s a file and the sender later `ftruncate`s it, the receiver
//! can crash with `SIGBUS` on access. To avoid that, the Linux memfd backend applies file seals:
//!
//! - `F_SEAL_SHRINK | F_SEAL_GROW` to lock the region size, and then
//! - `F_SEAL_SEAL` to lock the seal set.
//!
//! Locking the seal set is security-sensitive: an untrusted peer could otherwise add `F_SEAL_WRITE`
//! and permanently break reuse of pooled frame buffers (a persistent denial-of-service).
//!
//! ## FD inheritance notes (Linux)
//!
//! The Linux memfd backend creates the FD with `CLOEXEC` set by default (to avoid accidental FD
//! leaks). When spawning a renderer, the browser must clear `CLOEXEC` and tell the child which FD
//! number to use (via env var or CLI arg).
//!
//! Example (browser side):
//!
//! ```no_run
//! # use std::process::Command;
//! # use std::os::unix::process::CommandExt;
//! # use fastrender_shmem::{ShmemBackend, ShmemRegion};
//! # fn run() -> std::io::Result<()> {
//! let (_region, handle) = ShmemRegion::create(ShmemBackend::default(), 4096)?;
//! // Clear CLOEXEC immediately before exec so the FD is inherited by the renderer.
//! let fd = handle.fd().expect("memfd backend");
//! let len = handle.len();
//! let mut cmd = Command::new("fastrender-renderer");
//! cmd.env("FASTR_RENDER_SHMEM_FD", fd.to_string());
//! cmd.env("FASTR_RENDER_SHMEM_LEN", len.to_string());
//! // Safety: the `pre_exec` hook runs in the child after `fork` and before `exec`, so the closure
//! // must only perform async-signal-safe operations (here: `fcntl` via `clear_cloexec`).
//! unsafe {
//!   cmd.pre_exec(move || handle.clear_cloexec());
//! }
//! let _child = cmd.spawn()?;
//! # Ok(())
//! # }
//! ```
//!
//! The renderer then reads `FASTR_RENDER_SHMEM_FD`/`_LEN` and maps the region via
//! [`ShmemRegion::map`].
//!
//! ## Linux memfd seals (defense-in-depth)
//!
//! When the Linux memfd backend is used and the kernel supports sealing, FastRender applies
//! `F_SEAL_SHRINK | F_SEAL_GROW` and then locks the seal set with `F_SEAL_SEAL`.
//!
//! This prevents an untrusted renderer from *persistently* mutating seals (e.g. adding
//! `F_SEAL_WRITE`) in a way that would break pooled/shared buffers across subsequent frames.
//! Sealing is best-effort: if seals are unavailable due to kernel limitations or sandbox policy,
//! buffer creation still succeeds.
//!
//! If you also run a file-descriptor sanitizer in the child (e.g. "close all fds except stdio +
//! IPC"), remember to whitelist this memfd as well, otherwise the renderer will inherit the fd
//! number but find it closed by the time it tries to `mmap` it.

use memmap2::{MmapMut, MmapOptions};
use std::ffi::CString;
use std::fs::File;
use std::io;

#[cfg(unix)]
use std::os::unix::io::{AsRawFd, FromRawFd, RawFd};

/// Selects which shared-memory backend to use when creating a new region.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ShmemBackend {
  /// POSIX `shm_open` with a global name (typically backed by `/dev/shm`).
  ///
  /// This backend is easy to integrate but less desirable for sandboxed renderers because it
  /// requires allowing access to global shared-memory namespaces.
  #[cfg(unix)]
  PosixShm,

  /// Linux-only anonymous shared memory using `memfd_create` (or `O_TMPFILE` fallback).
  ///
  /// This backend avoids global namespaces: only processes that already possess the FD can map the
  /// region.
  #[cfg(target_os = "linux")]
  LinuxMemfd,
}

impl Default for ShmemBackend {
  fn default() -> Self {
    #[cfg(target_os = "linux")]
    {
      return ShmemBackend::LinuxMemfd;
    }
    #[cfg(all(unix, not(target_os = "linux")))]
    {
      return ShmemBackend::PosixShm;
    }
    #[cfg(not(unix))]
    {
      // Multiprocess shared memory is currently Unix-first.
      panic!("ShmemBackend::default is not supported on this platform");
    }
  }
}

/// A lightweight description of how another process can map a shared-memory region.
///
/// This is intentionally small so it can be passed over a control channel (CLI args, env vars,
/// pipes, etc.). The actual region backing must remain alive until the other process maps it.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ShmemHandle {
  /// A named POSIX shared-memory object (created/opened via `shm_open`).
  #[cfg(unix)]
  PosixShm {
    /// Name passed to `shm_open` (must start with `/`).
    name: String,
    /// Region size in bytes.
    len: usize,
  },

  /// A Linux memfd-backed region identified by an inherited file descriptor number.
  ///
  /// This is *not* an owning handle; it is just the numeric FD that should already be open in the
  /// target process (typically via FD inheritance across `exec`).
  #[cfg(target_os = "linux")]
  LinuxMemfd { fd: RawFd, len: usize },
}

impl ShmemHandle {
  pub fn len(&self) -> usize {
    match self {
      #[cfg(unix)]
      ShmemHandle::PosixShm { len, .. } => *len,
      #[cfg(target_os = "linux")]
      ShmemHandle::LinuxMemfd { len, .. } => *len,
    }
  }

  /// Returns the raw file descriptor for FD-backed handles.
  #[cfg(target_os = "linux")]
  pub fn fd(&self) -> Option<RawFd> {
    match self {
      ShmemHandle::LinuxMemfd { fd, .. } => Some(*fd),
      #[cfg(unix)]
      ShmemHandle::PosixShm { .. } => None,
    }
  }

  /// Clears `FD_CLOEXEC` on this handle's FD.
  ///
  /// Required when using the Linux memfd backend with FD inheritance: if `CLOEXEC` remains set,
  /// the FD will be closed during `exec` and the renderer won't be able to map the region.
  #[cfg(target_os = "linux")]
  pub fn clear_cloexec(&self) -> io::Result<()> {
    let Some(fd) = self.fd() else {
      return Ok(());
    };
    clear_fd_cloexec(fd)
  }
}

/// A mapped shared-memory region.
///
/// The mapping is mutable so it can be used as a frame buffer. Consumers that only need read-only
/// access can call [`ShmemRegion::as_slice`].
pub struct ShmemRegion {
  len: usize,
  mmap: MmapMut,
  #[allow(dead_code)]
  backend: ShmemRegionBackend,
}

#[allow(dead_code)]
enum ShmemRegionBackend {
  #[cfg(unix)]
  PosixShm { file: File, name: String },
  #[cfg(target_os = "linux")]
  LinuxMemfd { file: File },
}

impl ShmemRegion {
  /// Create a new shared-memory region using the requested backend.
  #[cfg(unix)]
  pub fn create(backend: ShmemBackend, len: usize) -> io::Result<(Self, ShmemHandle)> {
    match backend {
      #[cfg(unix)]
      ShmemBackend::PosixShm => Self::create_posix_shm(len),
      #[cfg(target_os = "linux")]
      ShmemBackend::LinuxMemfd => Self::create_linux_memfd(len),
      #[cfg(not(target_os = "linux"))]
      _ => Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "requested shmem backend is not supported on this platform",
      )),
    }
  }

  /// Map an existing region described by `handle`.
  ///
  /// For FD-backed handles this duplicates the FD with `CLOEXEC` set, so it is safe to call in the
  /// same process without affecting other owners of the original FD.
  #[cfg(unix)]
  pub fn map(handle: &ShmemHandle) -> io::Result<Self> {
    let len = handle.len();
    ensure_nonzero_len(len)?;
    match handle {
      #[cfg(unix)]
      ShmemHandle::PosixShm { name, len } => {
        let file = open_posix_shm(name, *len)?;
        let mmap = map_file_mut(&file, *len)?;
        Ok(Self {
          len: *len,
          mmap,
          backend: ShmemRegionBackend::PosixShm {
            file,
            name: name.clone(),
          },
        })
      }
      #[cfg(target_os = "linux")]
      ShmemHandle::LinuxMemfd { fd, len } => {
        let dup = dup_fd_cloexec(*fd)?;
        // SAFETY: `dup_fd_cloexec` returns a new owned file descriptor.
        let file = unsafe { File::from_raw_fd(dup) };
        let mmap = map_file_mut(&file, *len)?;
        Ok(Self {
          len: *len,
          mmap,
          backend: ShmemRegionBackend::LinuxMemfd { file },
        })
      }
    }
  }

  pub fn len(&self) -> usize {
    self.len
  }

  pub fn as_slice(&self) -> &[u8] {
    &self.mmap
  }

  pub fn as_mut_slice(&mut self) -> &mut [u8] {
    &mut self.mmap
  }

  #[cfg(unix)]
  fn create_posix_shm(len: usize) -> io::Result<(Self, ShmemHandle)> {
    ensure_nonzero_len(len)?;
    let name = generate_posix_shm_name();
    let file = create_posix_shm(&name, len)?;
    let mmap = map_file_mut(&file, len)?;
    let handle = ShmemHandle::PosixShm { name, len };
    let region = Self {
      len,
      mmap,
      backend: ShmemRegionBackend::PosixShm {
        file,
        name: match &handle {
          ShmemHandle::PosixShm { name, .. } => name.clone(),
          #[allow(unreachable_patterns)]
          _ => unreachable!(),
        },
      },
    };
    Ok((region, handle))
  }

  #[cfg(target_os = "linux")]
  fn create_linux_memfd(len: usize) -> io::Result<(Self, ShmemHandle)> {
    ensure_nonzero_len(len)?;
    let file = create_linux_memfd_file(len)?;
    let mmap = map_file_mut(&file, len)?;
    let handle = ShmemHandle::LinuxMemfd {
      fd: file.as_raw_fd(),
      len,
    };
    let region = Self {
      len,
      mmap,
      backend: ShmemRegionBackend::LinuxMemfd { file },
    };
    Ok((region, handle))
  }
}

fn ensure_nonzero_len(len: usize) -> io::Result<()> {
  if len == 0 {
    return Err(io::Error::new(
      io::ErrorKind::InvalidInput,
      "shared memory region length must be non-zero",
    ));
  }
  Ok(())
}

#[cfg(unix)]
fn map_file_mut(file: &File, len: usize) -> io::Result<MmapMut> {
  // SAFETY: The caller guarantees `len` bytes are valid in the backing file.
  unsafe { MmapOptions::new().len(len).map_mut(file) }
}

#[cfg(unix)]
fn dup_fd_cloexec(fd: RawFd) -> io::Result<RawFd> {
  // SAFETY: `fcntl` duplicates the file descriptor. We set CLOEXEC on the duplicate so it won't
  // leak into unrelated execs.
  let rc = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 3) };
  if rc < 0 {
    return Err(io::Error::last_os_error());
  }
  Ok(rc)
}

#[cfg(unix)]
fn generate_posix_shm_name() -> String {
  use std::sync::atomic::{AtomicU64, Ordering};
  static COUNTER: AtomicU64 = AtomicU64::new(0);
  let pid = std::process::id();
  let nonce = COUNTER.fetch_add(1, Ordering::Relaxed);
  format!("/fastrender-shm-{}-{}", pid, nonce)
}

#[cfg(unix)]
fn create_posix_shm(name: &str, len: usize) -> io::Result<File> {
  let c_name = CString::new(name).map_err(|_| {
    io::Error::new(
      io::ErrorKind::InvalidInput,
      "posix shm name contains an interior NUL byte",
    )
  })?;

  // SAFETY: `shm_open` is an FFI call; we pass a valid NUL-terminated name and standard flags.
  let fd = unsafe {
    libc::shm_open(
      c_name.as_ptr(),
      libc::O_CREAT | libc::O_EXCL | libc::O_RDWR | libc::O_CLOEXEC,
      0o600,
    )
  };
  if fd < 0 {
    return Err(io::Error::last_os_error());
  }

  truncate_fd(fd, len)?;

  // SAFETY: we just created `fd` and transfer ownership to `File`.
  Ok(unsafe { File::from_raw_fd(fd) })
}

#[cfg(unix)]
fn open_posix_shm(name: &str, len: usize) -> io::Result<File> {
  let c_name = CString::new(name).map_err(|_| {
    io::Error::new(
      io::ErrorKind::InvalidInput,
      "posix shm name contains an interior NUL byte",
    )
  })?;

  // SAFETY: `shm_open` is an FFI call; we pass a valid NUL-terminated name and standard flags.
  let fd = unsafe { libc::shm_open(c_name.as_ptr(), libc::O_RDWR | libc::O_CLOEXEC, 0) };
  if fd < 0 {
    return Err(io::Error::last_os_error());
  }

  // Ensure the backing region is large enough before mapping.
  truncate_fd(fd, len)?;

  // SAFETY: we just opened `fd` and transfer ownership to `File`.
  Ok(unsafe { File::from_raw_fd(fd) })
}

#[cfg(unix)]
fn truncate_fd(fd: RawFd, len: usize) -> io::Result<()> {
  let len_off: libc::off_t = len
    .try_into()
    .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "shmem length too large"))?;
  // SAFETY: `ftruncate` is an FFI call; we pass a valid fd and length.
  let rc = unsafe { libc::ftruncate(fd, len_off) };
  if rc != 0 {
    return Err(io::Error::last_os_error());
  }
  Ok(())
}

#[cfg(target_os = "linux")]
fn create_linux_memfd_file(len: usize) -> io::Result<File> {
  let c_name =
    CString::new("fastrender-shmem").expect("static memfd name must not contain NUL bytes");

  // SAFETY: `memfd_create` is a syscall/FFI boundary; we pass a valid NUL-terminated string and
  // Linux-defined flags.
  let mut fd =
    unsafe { libc::memfd_create(c_name.as_ptr(), libc::MFD_CLOEXEC | libc::MFD_ALLOW_SEALING) };
  if fd < 0 {
    let err = io::Error::last_os_error();
    // Older kernels may reject `MFD_ALLOW_SEALING` with EINVAL. Fall back to an unsealable memfd so
    // we can still allocate anonymous shared memory.
    if err.raw_os_error() == Some(libc::EINVAL) {
      fd = unsafe { libc::memfd_create(c_name.as_ptr(), libc::MFD_CLOEXEC) };
    }
  }

  if fd >= 0 {
    truncate_fd(fd, len)?;
    // Defense-in-depth: prevent untrusted peers from resizing the buffer and lock the seal set so
    // they cannot later add `F_SEAL_WRITE` (persistent DoS when buffers are pooled/reused).
    //
    // If seals are unsupported (e.g. unsealable memfd fallback or restrictive sandbox), this is a
    // best-effort no-op so allocation can still succeed.
    let _ = lock_linux_memfd_seals(fd);
    // SAFETY: we just created `fd` and transfer ownership to `File`.
    return Ok(unsafe { File::from_raw_fd(fd) });
  }

  let err = io::Error::last_os_error();
  // If memfd isn't available, fall back to `O_TMPFILE` to get an unnamed file descriptor.
  // Note that this still relies on filesystem support, so it may fail under strict sandboxes.
  let fallback_fd = create_otmpfile_fd().map_err(|fallback_err| {
    io::Error::new(
      fallback_err.kind(),
      format!(
        "memfd_create failed ({err}); O_TMPFILE fallback failed ({fallback_err})"
      ),
    )
  })?;
  truncate_fd(fallback_fd, len)?;
  // SAFETY: we just created `fallback_fd` and transfer ownership to `File`.
  Ok(unsafe { File::from_raw_fd(fallback_fd) })
}

#[cfg(target_os = "linux")]
fn lock_linux_memfd_seals(fd: RawFd) -> io::Result<()> {
  // If the fd doesn't support sealing (e.g. memfd created without MFD_ALLOW_SEALING), `F_ADD_SEALS`
  // will fail with EPERM and `F_GET_SEALS` may fail with EINVAL. Treat those as best-effort
  // unsupported rather than hard errors.
  let seals: libc::c_int = libc::F_SEAL_SHRINK | libc::F_SEAL_GROW | libc::F_SEAL_SEAL;
  // SAFETY: `fcntl(F_ADD_SEALS)` takes the fd and an int seal mask.
  let rc = unsafe { libc::fcntl(fd, libc::F_ADD_SEALS, seals) };
  if rc == 0 {
    return Ok(());
  }
  let err = io::Error::last_os_error();
  match err.raw_os_error() {
    Some(code) if code == libc::EPERM || code == libc::EINVAL => Ok(()),
    _ => Err(err),
  }
}

#[cfg(target_os = "linux")]
fn create_otmpfile_fd() -> io::Result<RawFd> {
  // Try tmpfs first for performance, then /tmp as a fallback.
  const DIRS: &[&str] = &["/dev/shm", "/tmp"];
  let mut last_err: Option<io::Error> = None;
  for dir in DIRS {
    let c_dir = match CString::new(*dir) {
      Ok(v) => v,
      Err(_) => continue,
    };
    // SAFETY: `open` is an FFI call; `c_dir` is NUL-terminated.
    let fd = unsafe {
      libc::open(
        c_dir.as_ptr(),
        libc::O_TMPFILE | libc::O_RDWR | libc::O_CLOEXEC,
        0o600,
      )
    };
    if fd >= 0 {
      return Ok(fd);
    }
    last_err = Some(io::Error::last_os_error());
  }

  Err(last_err.unwrap_or_else(|| {
    io::Error::new(
      io::ErrorKind::NotFound,
      "no suitable directory for O_TMPFILE",
    )
  }))
}

#[cfg(target_os = "linux")]
fn clear_fd_cloexec(fd: RawFd) -> io::Result<()> {
  // SAFETY: `fcntl` reads per-fd flags.
  let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
  if flags < 0 {
    return Err(io::Error::last_os_error());
  }
  let new_flags = flags & !libc::FD_CLOEXEC;
  // SAFETY: `fcntl` sets per-fd flags.
  let rc = unsafe { libc::fcntl(fd, libc::F_SETFD, new_flags) };
  if rc < 0 {
    return Err(io::Error::last_os_error());
  }
  Ok(())
}

#[cfg(test)]
mod tests {
  use super::*;

  #[cfg(target_os = "linux")]
  #[test]
  fn linux_memfd_backend_maps_in_process() {
    let (mut region, handle) =
      ShmemRegion::create(ShmemBackend::LinuxMemfd, 4096).expect("create memfd shmem");

    // memfd is created with CLOEXEC by default.
    let fd = handle.fd().expect("memfd handle fd");
    let initial_flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
    assert!(initial_flags & libc::FD_CLOEXEC != 0);

    // If sealing is supported, we lock the seal set (F_SEAL_SEAL) and prevent resizing.
    let seals = unsafe { libc::fcntl(fd, libc::F_GET_SEALS) };
    if seals >= 0 {
      assert!(seals & libc::F_SEAL_SHRINK != 0);
      assert!(seals & libc::F_SEAL_GROW != 0);
      assert!(seals & libc::F_SEAL_SEAL != 0);
    }

    handle.clear_cloexec().expect("clear cloexec on memfd");
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
    assert_eq!(flags & libc::FD_CLOEXEC, 0);

    region.as_mut_slice()[0..4].copy_from_slice(b"FRSH");

    let mut other = ShmemRegion::map(&handle).expect("map memfd handle");
    assert_eq!(&other.as_slice()[0..4], b"FRSH");

    other.as_mut_slice()[1] = b'!';
    assert_eq!(&region.as_slice()[0..4], b"F!SH");
  }

  #[cfg(target_os = "linux")]
  #[test]
  fn linux_memfd_backend_locks_seals_to_prevent_mutation() {
    let (_region, handle) =
      ShmemRegion::create(ShmemBackend::LinuxMemfd, 4096).expect("create memfd shmem");
    let fd = handle.fd().expect("memfd handle fd");

    // SAFETY: `fcntl(F_GET_SEALS)` takes no extra arguments.
    let seals = unsafe { libc::fcntl(fd, libc::F_GET_SEALS) };
    if seals < 0 {
      // Seals may be unavailable due to kernel limitations (e.g. memfd without sealing) or
      // restrictive sandbox policy. Treat this as best-effort: other tests cover the happy path.
      return;
    }
    assert_eq!(
      seals & (libc::F_SEAL_SHRINK | libc::F_SEAL_GROW),
      libc::F_SEAL_SHRINK | libc::F_SEAL_GROW,
      "expected size seals to be applied"
    );
    assert_ne!(seals & libc::F_SEAL_SEAL, 0, "expected seals to be locked");

    // Once the seal set is locked, untrusted peers must not be able to add F_SEAL_WRITE.
    let rc = unsafe { libc::fcntl(fd, libc::F_ADD_SEALS, libc::F_SEAL_WRITE) };
    assert_eq!(rc, -1);
    assert_eq!(std::io::Error::last_os_error().raw_os_error(), Some(libc::EPERM));
  }
}
