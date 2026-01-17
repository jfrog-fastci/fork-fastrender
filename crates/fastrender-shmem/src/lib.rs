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
//! # #[cfg(unix)]
//! # use std::os::unix::process::CommandExt;
//! # use fastrender_shmem::{ShmemBackend, ShmemRegion};
//! # #[cfg(target_os = "linux")]
//! # fn run() -> std::io::Result<()> {
//! let (_region, handle) = ShmemRegion::create(ShmemBackend::LinuxMemfd, 4096)?;
//! // Clear CLOEXEC immediately before exec so the FD is inherited by the renderer.
//! let len = handle.len();
//! let mut cmd = Command::new("fastrender-renderer");
//! // Use a stable, non-conflicting FD number for the inherited memfd.
//! // (If you also inherit an IPC socket at fd=3, prefer fd=4 or higher here.)
//! const SHMEM_FD: i32 = 4;
//! cmd.env("FASTR_RENDER_SHMEM_FD", SHMEM_FD.to_string());
//! cmd.env("FASTR_RENDER_SHMEM_LEN", len.to_string());
//! // Safety: the `pre_exec` hook runs in the child after `fork` and before `exec`, so the closure
//! // must only perform async-signal-safe operations (here: `dup`/`dup2`/`close`).
//! unsafe {
//!   cmd.pre_exec(move || handle.dup_to_fd(SHMEM_FD));
//! }
//! let _child = cmd.spawn()?;
//! # Ok(())
//! # }
//! #
//! # #[cfg(not(target_os = "linux"))]
//! # fn run() -> std::io::Result<()> { Ok(()) }
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

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use getrandom::getrandom;
use memmap2::MmapMut;
#[cfg(unix)]
use memmap2::MmapOptions;
#[cfg(unix)]
use std::ffi::CString;
#[cfg(unix)]
use std::fs::File;
#[cfg(unix)]
use std::io;

#[cfg(unix)]
use std::os::unix::io::{AsRawFd, FromRawFd, RawFd};

/// Maximum length of the OS-level name passed to `shm_open`, including the leading `/`.
///
/// Many platforms allow longer names, but macOS commonly enforces `PSHMNAMLEN = 31` bytes.
/// We keep the strictest known limit globally to avoid portability bugs.
pub const MAX_SHMEM_NAME_LEN: usize = 31;

/// Maximum length of the user-facing shared-memory identifier (without the leading `/`).
pub const MAX_SHMEM_ID_LEN: usize = MAX_SHMEM_NAME_LEN - 1;

// Keep this prefix extremely short so the random payload fits under macOS's strict limit.
const SHMEM_ID_PREFIX: &str = "fr";
const SHMEM_ID_RANDOM_BYTES: usize = 16; // 128 bits

/// Generates a fresh shared-memory identifier suitable for POSIX `shm_open`.
///
/// ## Security motivation
/// In FastRender's multiprocess model, sandboxed renderers typically run under the same UID as the
/// browser. A compromised renderer that can guess another tab's shared-memory identifier can open
/// and read/write its contents (owner-only mode bits don't help against same-UID attackers).
///
/// Therefore, identifiers must be **unguessable**: we use 128 bits of OS randomness.
///
/// ## Invariants
/// - ASCII-only, with a short stable prefix.
/// - Contains only URL-safe Base64 characters (`[A-Za-z0-9_-]`) and contains no `/`.
/// - Does **not** include a leading `/`; POSIX requires one for `shm_open`, but we add it only at
///   the OS call-site.
/// - Total length is capped at [`MAX_SHMEM_ID_LEN`] (30 bytes today) so the final `shm_open` name
///   including the required leading `/` fits within [`MAX_SHMEM_NAME_LEN`] (31 bytes). This matches
///   macOS's common `PSHMNAMLEN = 31` limit.
///
/// ## Panics
/// Panics if OS randomness is unavailable. Falling back to predictable identifiers would defeat
/// the security boundary between renderer processes.
pub fn generate_shmem_id() -> String {
  let mut rand_bytes = [0u8; SHMEM_ID_RANDOM_BYTES];
  getrandom(&mut rand_bytes).expect("failed to obtain OS randomness for shared memory id");

  let encoded = URL_SAFE_NO_PAD.encode(rand_bytes);
  let mut out = String::with_capacity(SHMEM_ID_PREFIX.len() + encoded.len());
  out.push_str(SHMEM_ID_PREFIX);
  out.push_str(&encoded);
  // Adding the leading `/` happens at the syscall layer; keep IDs strictly under the OS limit.
  assert!(
    out.len() <= MAX_SHMEM_ID_LEN,
    "generated shm id too long: {} bytes (max {})",
    out.len(),
    MAX_SHMEM_ID_LEN
  );
  debug_assert!(out.is_ascii());
  debug_assert!(!out.contains('/'));
  out
}

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
    /// Shared-memory identifier (stored **without** the leading `/` required by `shm_open`).
    id: String,
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
      #[cfg(not(unix))]
      _ => unreachable!("ShmemHandle is not supported on non-Unix platforms"),
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

  /// Duplicate this handle's FD onto `target_fd` and ensure it is *not* `CLOEXEC`.
  ///
  /// This is a convenience helper for FD inheritance across `exec`: callers can choose a stable FD
  /// number (e.g. `3` for IPC, `4` for shared memory) and set an env/arg accordingly.
  ///
  /// This is designed to be called from `CommandExt::pre_exec` in the browser process.
  #[cfg(target_os = "linux")]
  pub fn dup_to_fd(&self, target_fd: RawFd) -> io::Result<()> {
    let Some(fd) = self.fd() else {
      return Ok(());
    };

    // `dup2` clears `FD_CLOEXEC` on the new descriptor, except when `fd == target_fd` (no-op). To
    // handle the equality case without relying on `fcntl` (useful for `pre_exec`), duplicate to a
    // temporary fd first and then `dup2` onto `target_fd`.
    if fd == target_fd {
      // SAFETY: `dup` is an FFI boundary; on success it returns a new fd referring to the same file
      // description with `FD_CLOEXEC` cleared.
      let tmp_fd = unsafe { libc::dup(fd) };
      if tmp_fd < 0 {
        return Err(io::Error::last_os_error());
      }

      // SAFETY: `dup2` duplicates `tmp_fd` onto `target_fd` (closing `target_fd` first if needed).
      let rc = unsafe { libc::dup2(tmp_fd, target_fd) };
      let dup2_err = if rc < 0 {
        Some(io::Error::last_os_error())
      } else {
        None
      };

      // Always close the temporary fd to avoid leaking it across exec.
      loop {
        // SAFETY: `close` is an FFI boundary; on success it closes `tmp_fd`.
        let close_rc = unsafe { libc::close(tmp_fd) };
        if close_rc == 0 {
          break;
        }
        let err = io::Error::last_os_error();
        if err.raw_os_error() == Some(libc::EINTR) {
          continue;
        }
        // Prefer returning the `dup2` error if both failed.
        return Err(dup2_err.unwrap_or(err));
      }

      if let Some(err) = dup2_err {
        return Err(err);
      }
      return Ok(());
    }

    // SAFETY: `dup2` is an FFI boundary; on success it duplicates `fd` onto `target_fd` in the
    // current process (closing the previous `target_fd` if it was open).
    let rc = unsafe { libc::dup2(fd, target_fd) };
    if rc < 0 {
      return Err(io::Error::last_os_error());
    }
    Ok(())
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
  PosixShm {
    id: String,
    creator: bool,
  },
  #[cfg(target_os = "linux")]
  LinuxMemfd { file: File },
}

impl ShmemRegion {
  /// Create a new shared-memory region using the requested backend.
  #[cfg(unix)]
  #[allow(unreachable_patterns)]
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
      ShmemHandle::PosixShm { id, len } => {
        let file = open_posix_shm(id, *len)?;
        let mmap = map_file_mut(&file, *len)?;
        Ok(Self {
          len: *len,
          mmap,
          backend: ShmemRegionBackend::PosixShm {
            id: id.clone(),
            creator: false,
          },
        })
      }
      #[cfg(target_os = "linux")]
      ShmemHandle::LinuxMemfd { fd, len } => {
        let dup = dup_fd_cloexec(*fd)?;
        // SAFETY: `dup_fd_cloexec` returns a new owned file descriptor.
        let file = unsafe { File::from_raw_fd(dup) };
        validate_fd_size_and_type(file.as_raw_fd(), *len)?;
        // Defense-in-depth: ensure size-stability seals are applied before mapping if possible.
        // This is best-effort; some kernels/sandboxes may make the fd unsealable.
        let _ = lock_linux_memfd_seals(file.as_raw_fd());
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

    // Even with 128 bits of randomness, handle the (extremely unlikely) EEXIST case gracefully.
    for _ in 0..32 {
      let id = generate_shmem_id();
      let (file, name) = match create_posix_shm_file(&id, len) {
        Ok(v) => v,
        Err(err) if err.raw_os_error() == Some(libc::EEXIST) => continue,
        Err(err) => return Err(err),
      };

      // If mapping fails, ensure the named object doesn't leak.
      let mut unlink_guard = PosixUnlinkGuard::new(name);
      let mut mmap = match map_file_mut(&file, len) {
        Ok(m) => m,
        Err(err) => return Err(err),
      };
      // Security: make it explicit that newly created shared-memory regions begin zeroed. Even
      // though most OS APIs provide zeroed pages for new mappings, keeping the invariant explicit
      // avoids leaking previous-process memory (or stale named-shm contents if an ID is ever reused
      // accidentally) to an untrusted renderer.
      mmap.fill(0);
      unlink_guard.disarm();

      let handle = ShmemHandle::PosixShm { id: id.clone(), len };
      let region = Self {
        len,
        mmap,
        backend: ShmemRegionBackend::PosixShm {
          id,
          creator: true,
        },
      };
      return Ok((region, handle));
    }

    Err(io::Error::new(
      io::ErrorKind::AlreadyExists,
      "failed to create a unique POSIX shared memory name after multiple attempts",
    ))
  }

  #[cfg(target_os = "linux")]
  fn create_linux_memfd(len: usize) -> io::Result<(Self, ShmemHandle)> {
    ensure_nonzero_len(len)?;
    let file = create_linux_memfd_file(len)?;
    let mut mmap = map_file_mut(&file, len)?;
    // Security: explicitly zero the mapping to avoid leaking stale bytes if the kernel ever
    // provides non-zeroed pages (or if the backing fd/name is accidentally reused).
    mmap.fill(0);
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

impl Drop for ShmemRegion {
  fn drop(&mut self) {
    #[cfg(unix)]
    {
      if let ShmemRegionBackend::PosixShm { id, creator: true, .. } = &self.backend {
        // Best-effort cleanup. If this fails there is nothing meaningful we can do from Drop.
        if let Ok(name) = posix_shm_name(id) {
          unsafe {
            libc::shm_unlink(name.as_ptr());
          }
        }
      }
    }
  }
}

#[cfg(unix)]
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
#[allow(dead_code)]
fn validate_fd_size_and_type(fd: RawFd, expected_len: usize) -> io::Result<()> {
  let mut st: libc::stat = unsafe { std::mem::zeroed() };
  // SAFETY: `fstat` writes to `st` when the pointer is valid.
  let rc = unsafe { libc::fstat(fd, &mut st) };
  if rc != 0 {
    return Err(io::Error::last_os_error());
  }

  // `S_ISREG` is a macro in libc; implement the check directly to avoid relying on macro exports.
  if (st.st_mode & libc::S_IFMT) != libc::S_IFREG {
    return Err(io::Error::new(
      io::ErrorKind::InvalidInput,
      format!("shmem fd is not a regular file (mode=0o{:o})", st.st_mode),
    ));
  }

  if st.st_size < 0 {
    return Err(io::Error::new(
      io::ErrorKind::InvalidData,
      format!("shmem fd reported a negative size ({})", st.st_size),
    ));
  }
  let actual_len: u64 = st.st_size as u64;
  let expected_len_u64: u64 = expected_len
    .try_into()
    .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "shmem length too large"))?;

  if actual_len != expected_len_u64 {
    return Err(io::Error::new(
      io::ErrorKind::InvalidData,
      format!("shared memory size mismatch: expected {expected_len_u64}, got {actual_len}"),
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
#[allow(dead_code)]
fn dup_fd_cloexec(fd: RawFd) -> io::Result<RawFd> {
  // SAFETY: `fcntl` duplicates the file descriptor. We set CLOEXEC on the duplicate so it won't
  // leak into unrelated execs.
  loop {
    let rc = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 3) };
    if rc >= 0 {
      return Ok(rc);
    }
    let err = io::Error::last_os_error();
    if err.kind() == io::ErrorKind::Interrupted {
      continue;
    }
    return Err(err);
  }
}

#[cfg(unix)]
fn posix_shm_name(id: &str) -> io::Result<CString> {
  if id.is_empty() {
    return Err(io::Error::new(
      io::ErrorKind::InvalidInput,
      "posix shm id must not be empty",
    ));
  }
  if id.len() > MAX_SHMEM_ID_LEN {
    return Err(io::Error::new(
      io::ErrorKind::InvalidInput,
      format!("posix shm id too long: {} bytes (max {MAX_SHMEM_ID_LEN})", id.len()),
    ));
  }

  // Keep names in a conservative portable charset:
  // - ASCII only
  // - no path separators (`/`)
  // - no whitespace or control chars
  if !id
    .as_bytes()
    .iter()
    .all(|b| matches!(b, b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_'))
  {
    return Err(io::Error::new(
      io::ErrorKind::InvalidInput,
      "posix shm id must contain only ASCII [A-Za-z0-9_-]",
    ));
  }
  if id.as_bytes().iter().any(|b| *b == b'/' || *b == 0) {
    return Err(io::Error::new(
      io::ErrorKind::InvalidInput,
      "posix shm id must not contain '/' or NUL",
    ));
  }
  CString::new(format!("/{id}")).map_err(|_| {
    io::Error::new(
      io::ErrorKind::InvalidInput,
      "posix shm id contained an interior NUL byte",
    )
  })
}

#[cfg(unix)]
fn create_posix_shm_file(id: &str, len: usize) -> io::Result<(File, CString)> {
  let name = posix_shm_name(id)?;

  // SAFETY: `shm_open` is an FFI call; we pass a valid NUL-terminated name and standard flags.
  let fd = unsafe {
    libc::shm_open(
      name.as_ptr(),
      libc::O_CREAT | libc::O_EXCL | libc::O_RDWR | libc::O_CLOEXEC,
      0o600,
    )
  };
  if fd < 0 {
    return Err(io::Error::last_os_error());
  }

  // SAFETY: we just created `fd` and transfer ownership to `File`.
  let file = unsafe { File::from_raw_fd(fd) };

  // Ensure we don't leak the object if sizing fails.
  if let Err(err) = truncate_fd(file.as_raw_fd(), len) {
    unsafe {
      libc::shm_unlink(name.as_ptr());
    }
    return Err(err);
  }

  Ok((file, name))
}

#[cfg(unix)]
fn open_posix_shm(id: &str, len: usize) -> io::Result<File> {
  let name = posix_shm_name(id)?;

  // SAFETY: `shm_open` is an FFI call; we pass a valid NUL-terminated name and standard flags.
  let fd = unsafe { libc::shm_open(name.as_ptr(), libc::O_RDWR | libc::O_CLOEXEC, 0) };
  if fd < 0 {
    return Err(io::Error::last_os_error());
  }

  // SAFETY: we just opened `fd` and transfer ownership to `File`.
  let file = unsafe { File::from_raw_fd(fd) };

  // Validate the backing region size before mapping. Mapping past the end of a shared-memory object
  // can cause SIGBUS on access.
  let actual_len = fd_len(file.as_raw_fd())?;
  if actual_len != len {
    return Err(io::Error::new(
      io::ErrorKind::InvalidData,
      format!(
        "posix shm size mismatch for id={id}: expected {len} bytes, got {actual_len} bytes"
      ),
    ));
  }

  Ok(file)
}

#[cfg(unix)]
struct PosixUnlinkGuard {
  name: CString,
  armed: bool,
}

#[cfg(unix)]
impl PosixUnlinkGuard {
  fn new(name: CString) -> Self {
    Self { name, armed: true }
  }

  fn disarm(&mut self) {
    self.armed = false;
  }
}

#[cfg(unix)]
impl Drop for PosixUnlinkGuard {
  fn drop(&mut self) {
    if self.armed {
      // Best-effort; nothing meaningful can be done on failure.
      unsafe {
        libc::shm_unlink(self.name.as_ptr());
      }
    }
  }
}

#[cfg(unix)]
fn fd_len(fd: RawFd) -> io::Result<usize> {
  let mut st = std::mem::MaybeUninit::<libc::stat>::uninit();
  // SAFETY: `fstat` writes a fully-initialized `stat` on success.
  let rc = unsafe { libc::fstat(fd, st.as_mut_ptr()) };
  if rc != 0 {
    return Err(io::Error::last_os_error());
  }
  // SAFETY: initialized by successful `fstat`.
  let st = unsafe { st.assume_init() };
  let size: u64 = st
    .st_size
    .try_into()
    .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "posix shm size overflow"))?;
  usize::try_from(size).map_err(|_| {
    io::Error::new(
      io::ErrorKind::InvalidData,
      "posix shm size does not fit in usize",
    )
  })
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
  // If the fd doesn't support sealing (e.g. memfd created without `MFD_ALLOW_SEALING` or restricted
  // by sandbox policy), `F_ADD_SEALS` fails with EPERM/EINVAL/ENOSYS. Treat those as best-effort
  // unsupported rather than hard errors.
  //
  // Apply the required size-stability seals first so they still take effect on kernels that can't
  // lock the seal set (`F_SEAL_SEAL`).
  let required: libc::c_int = libc::F_SEAL_SHRINK | libc::F_SEAL_GROW;
  loop {
    // SAFETY: `fcntl(F_ADD_SEALS)` takes the fd and an int seal mask.
    let rc = unsafe { libc::fcntl(fd, libc::F_ADD_SEALS, required) };
    if rc == 0 {
      break;
    }
    let err = io::Error::last_os_error();
    if err.kind() == io::ErrorKind::Interrupted {
      continue;
    }
    match err.raw_os_error() {
      Some(code) if code == libc::EPERM || code == libc::EINVAL || code == libc::ENOSYS => return Ok(()),
      _ => return Err(err),
    }
  }

  // Best-effort: lock the seal set so an untrusted peer cannot persistently add restrictive seals
  // like `F_SEAL_WRITE` (persistent DoS when buffers are pooled).
  loop {
    let rc = unsafe { libc::fcntl(fd, libc::F_ADD_SEALS, libc::F_SEAL_SEAL) };
    if rc == 0 {
      return Ok(());
    }
    let err = io::Error::last_os_error();
    if err.kind() == io::ErrorKind::Interrupted {
      continue;
    }
    return match err.raw_os_error() {
      Some(code) if code == libc::EPERM || code == libc::EINVAL || code == libc::ENOSYS => Ok(()),
      _ => Err(err),
    };
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
  let flags = loop {
    // SAFETY: `fcntl` reads per-fd flags.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
    if flags >= 0 {
      break flags;
    }
    let err = io::Error::last_os_error();
    if err.kind() == io::ErrorKind::Interrupted {
      continue;
    }
    return Err(err);
  };
  let new_flags = flags & !libc::FD_CLOEXEC;
  loop {
    // SAFETY: `fcntl` sets per-fd flags.
    let rc = unsafe { libc::fcntl(fd, libc::F_SETFD, new_flags) };
    if rc >= 0 {
      break;
    }
    let err = io::Error::last_os_error();
    if err.kind() == io::ErrorKind::Interrupted {
      continue;
    }
    return Err(err);
  }
  Ok(())
}

#[cfg(test)]
mod tests {
  use super::*;
  use std::collections::HashSet;
  #[cfg(target_os = "linux")]
  use std::os::unix::process::CommandExt as _;

  #[cfg(target_os = "linux")]
  #[test]
  fn linux_memfd_backend_maps_in_process() {
    let (mut region, handle) =
      ShmemRegion::create(ShmemBackend::LinuxMemfd, 4096).expect("create memfd shmem");

    // memfd is created with CLOEXEC by default.
    let fd = handle.fd().expect("memfd handle fd");
    let initial_flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
    assert!(initial_flags & libc::FD_CLOEXEC != 0);

    // If sealing is supported, we prevent resizing (`F_SEAL_SHRINK|F_SEAL_GROW`). Locking the seal
    // set (`F_SEAL_SEAL`) is best-effort and may be blocked by sandbox policy, so don't require it
    // here.
    let seals = unsafe { libc::fcntl(fd, libc::F_GET_SEALS) };
    if seals >= 0 {
      assert!(seals & libc::F_SEAL_SHRINK != 0);
      assert!(seals & libc::F_SEAL_GROW != 0);
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
  fn linux_memfd_map_applies_size_seals_when_possible() {
    let c_name = CString::new("fastrender-shmem-test-seals")
      .expect("static memfd name must not contain NUL bytes");
    // SAFETY: `memfd_create` is a syscall/FFI boundary; `c_name` is NUL-terminated.
    let fd =
      unsafe { libc::memfd_create(c_name.as_ptr(), libc::MFD_CLOEXEC | libc::MFD_ALLOW_SEALING) };
    if fd < 0 {
      let err = io::Error::last_os_error();
      if err.raw_os_error() == Some(libc::EINVAL) {
        // Older kernels may not support `MFD_ALLOW_SEALING`; nothing to validate here.
        return;
      }
      panic!("memfd_create failed: {err}");
    }

    truncate_fd(fd, 4096).expect("truncate memfd");
    // SAFETY: `fd` is freshly created and owned by us.
    let file = unsafe { File::from_raw_fd(fd) };

    // This memfd is intentionally unsealed; `ShmemRegion::map` should best-effort apply size seals.
    let handle = ShmemHandle::LinuxMemfd {
      fd: file.as_raw_fd(),
      len: 4096,
    };

    // SAFETY: `fcntl(F_GET_SEALS)` takes no extra arguments.
    let seals_before = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GET_SEALS) };
    if seals_before >= 0 {
      assert_eq!(
        seals_before & (libc::F_SEAL_SHRINK | libc::F_SEAL_GROW),
        0,
        "expected test memfd to start without size seals"
      );
    }

    let _region = ShmemRegion::map(&handle).expect("map memfd handle");
    let seals_after = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GET_SEALS) };
    if seals_after >= 0 {
      assert!(seals_after & libc::F_SEAL_SHRINK != 0);
      assert!(seals_after & libc::F_SEAL_GROW != 0);
      assert!(seals_after & libc::F_SEAL_SEAL != 0);
    }
  }

  #[cfg(target_os = "linux")]
  #[test]
  fn linux_memfd_create_zero_initializes_mapping() {
    let (region, _handle) =
      ShmemRegion::create(ShmemBackend::LinuxMemfd, 128).expect("create memfd shmem");
    assert!(
      region.as_slice().iter().all(|b| *b == 0),
      "newly created shared-memory region should be zero-initialized"
    );
  }

  #[cfg(all(unix, not(target_os = "linux")))]
  #[test]
  fn posix_shm_create_zero_initializes_mapping() {
    let (region, _handle) = ShmemRegion::create(ShmemBackend::PosixShm, 128).expect("create shmem");
    assert!(
      region.as_slice().iter().all(|b| *b == 0),
      "newly created shared-memory region should be zero-initialized"
    );
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
    let required = libc::F_SEAL_SHRINK | libc::F_SEAL_GROW;
    if (seals & required) != required {
      // Some sandboxes may allow `F_GET_SEALS` but deny `F_ADD_SEALS` (EPERM), leaving seals unset.
      // Treat sealing as best-effort for this crate; other integration tests cover strict modes.
      return;
    }

    // `F_SEAL_SEAL` is defense-in-depth: when supported it prevents untrusted peers from adding
    // restrictive seals (e.g. `F_SEAL_WRITE`) to pooled buffers.
    if (seals & libc::F_SEAL_SEAL) == 0 {
      // If we can lock seals now, the backend should have done so during creation.
      let rc = unsafe { libc::fcntl(fd, libc::F_ADD_SEALS, libc::F_SEAL_SEAL) };
      if rc == 0 {
        panic!("expected Linux memfd backend to lock the seal set with F_SEAL_SEAL");
      }
      let err = std::io::Error::last_os_error();
      match err.raw_os_error() {
        Some(libc::EINVAL) | Some(libc::ENOSYS) | Some(libc::EPERM) => return,
        _ => panic!("unexpected error attempting to add F_SEAL_SEAL: {err}"),
      }
    }

    // Once the seal set is locked, untrusted peers must not be able to add F_SEAL_WRITE.
    let rc = unsafe { libc::fcntl(fd, libc::F_ADD_SEALS, libc::F_SEAL_WRITE) };
    assert_eq!(rc, -1);
    assert_eq!(std::io::Error::last_os_error().raw_os_error(), Some(libc::EPERM));
  }

  #[cfg(target_os = "linux")]
  #[test]
  fn linux_memfd_dup_to_fd_clears_cloexec() {
    let (mut region, handle) =
      ShmemRegion::create(ShmemBackend::LinuxMemfd, 4096).expect("create memfd shmem");
    region.as_mut_slice()[0..4].copy_from_slice(b"TEST");

    let src_fd = handle.fd().expect("memfd handle fd");
    let target_fd = unsafe { libc::fcntl(src_fd, libc::F_DUPFD_CLOEXEC, 100) };
    assert!(target_fd >= 0, "F_DUPFD_CLOEXEC failed: {}", std::io::Error::last_os_error());

    let flags = unsafe { libc::fcntl(target_fd, libc::F_GETFD) };
    assert!(flags & libc::FD_CLOEXEC != 0);

    handle
      .dup_to_fd(target_fd)
      .expect("dup memfd to target fd and clear cloexec");
    let flags = unsafe { libc::fcntl(target_fd, libc::F_GETFD) };
    assert_eq!(flags & libc::FD_CLOEXEC, 0);

    let dup_handle = ShmemHandle::LinuxMemfd {
      fd: target_fd,
      len: handle.len(),
    };
    let other = ShmemRegion::map(&dup_handle).expect("map dup fd handle");
    assert_eq!(&other.as_slice()[0..4], b"TEST");

    // Clean up our manually-duplicated fd.
    unsafe {
      libc::close(target_fd);
    }
  }

  #[cfg(target_os = "linux")]
  #[test]
  fn linux_memfd_dup_to_same_fd_clears_cloexec() {
    let (_region, handle) =
      ShmemRegion::create(ShmemBackend::LinuxMemfd, 4096).expect("create memfd shmem");
    let fd = handle.fd().expect("memfd handle fd");

    let initial_flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
    assert!(initial_flags & libc::FD_CLOEXEC != 0);

    handle
      .dup_to_fd(fd)
      .expect("dup memfd onto same fd number and clear cloexec");
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
    assert_eq!(flags & libc::FD_CLOEXEC, 0);
  }

  #[cfg(unix)]
  #[test]
  fn posix_shm_map_rejects_size_mismatch() {
    let id = generate_shmem_id();
    let (_file, name) = create_posix_shm_file(&id, 4097).expect("create posix shm");
    let _guard = PosixUnlinkGuard::new(name);

    let handle = ShmemHandle::PosixShm { id, len: 4096 };
    match ShmemRegion::map(&handle) {
      Ok(_) => panic!("expected map to fail due to size mismatch"),
      Err(err) => assert_eq!(err.kind(), io::ErrorKind::InvalidData),
    }
  }

  #[cfg(target_os = "linux")]
  #[test]
  fn linux_memfd_map_rejects_size_mismatch() {
    let file = create_linux_memfd_file(4096).expect("create memfd file");
    let handle = ShmemHandle::LinuxMemfd {
      fd: file.as_raw_fd(),
      len: 4097,
    };
    match ShmemRegion::map(&handle) {
      Ok(_) => panic!("expected map to fail due to size mismatch"),
      Err(err) => assert_eq!(err.kind(), io::ErrorKind::InvalidData),
    }
  }

  #[test]
  fn generate_shmem_id_is_unique_over_many_iterations() {
    let mut seen = HashSet::new();
    for _ in 0..1024 {
      let id = generate_shmem_id();
      assert!(seen.insert(id), "duplicate shared memory id generated");
    }
  }

  #[test]
  fn generate_shmem_id_is_ascii_safe_and_non_empty() {
    for _ in 0..256 {
      let id = generate_shmem_id();
      assert!(!id.is_empty());
      assert!(id.starts_with(SHMEM_ID_PREFIX));
      assert!(id.is_ascii());
      assert!(
        id.len() <= MAX_SHMEM_ID_LEN,
        "id length {} exceeded MAX_SHMEM_ID_LEN={MAX_SHMEM_ID_LEN}",
        id.len()
      );
      assert!(
        id.as_bytes().iter().all(|b| matches!(
          b,
          b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_'
        )),
        "id contains unexpected characters: {id:?}"
      );
      assert!(!id.contains('/'), "id must not contain '/': {id:?}");

      #[cfg(unix)]
      {
        let name = posix_shm_name(&id).expect("posix_shm_name should accept generated ids");
        assert!(
          name.as_bytes().len() <= MAX_SHMEM_NAME_LEN,
          "shm_open name length {} exceeded MAX_SHMEM_NAME_LEN={MAX_SHMEM_NAME_LEN}",
          name.as_bytes().len()
        );
      }
    }
  }

  #[cfg(unix)]
  #[test]
  fn posix_shm_name_rejects_invalid_ids() {
    // Empty.
    assert!(posix_shm_name("").is_err());

    // Too long.
    let too_long = "a".repeat(MAX_SHMEM_ID_LEN + 1);
    assert!(posix_shm_name(&too_long).is_err());

    // Invalid charset.
    assert!(posix_shm_name("has/slash").is_err());
    assert!(posix_shm_name("has.dot").is_err());
    assert!(posix_shm_name("has space").is_err());
    assert!(posix_shm_name("has:colon").is_err());

    // Valid.
    posix_shm_name("has_underscore").expect("underscore should be allowed");
    posix_shm_name("HasUppercase").expect("uppercase should be allowed");
    let ok = posix_shm_name("fastrender-shm-deadbeef").expect("valid posix shm name");
    assert_eq!(ok.to_bytes()[0], b'/');
    assert_eq!(ok.to_bytes(), b"/fastrender-shm-deadbeef");
  }

  #[cfg(unix)]
  #[test]
  fn posix_shm_open_rejects_size_mismatch() {
    let (_region, handle) =
      ShmemRegion::create(ShmemBackend::PosixShm, 4096).expect("create posix shm");
    let ShmemHandle::PosixShm { id, .. } = handle else {
      panic!("expected posix shm handle");
    };

    let wrong = ShmemHandle::PosixShm { id, len: 4097 };
    let err = match ShmemRegion::map(&wrong) {
      Ok(_) => panic!("expected size mismatch"),
      Err(err) => err,
    };
    assert_eq!(err.kind(), io::ErrorKind::InvalidData);
  }

  #[cfg(unix)]
  #[test]
  fn posix_shm_unlinked_when_creator_drops() {
    let (region, handle) =
      ShmemRegion::create(ShmemBackend::PosixShm, 4096).expect("create posix shm");
    let ShmemHandle::PosixShm { id, .. } = handle else {
      panic!("expected posix shm handle");
    };
    let name = posix_shm_name(&id).expect("posix shm name");

    // The name should exist while the creator region is alive.
    let fd = unsafe { libc::shm_open(name.as_ptr(), libc::O_RDWR | libc::O_CLOEXEC, 0) };
    assert!(
      fd >= 0,
      "shm_open should succeed while region is alive: {}",
      io::Error::last_os_error()
    );
    unsafe {
      libc::close(fd);
    }

    drop(region);

    let fd2 = unsafe { libc::shm_open(name.as_ptr(), libc::O_RDWR | libc::O_CLOEXEC, 0) };
    if fd2 >= 0 {
      unsafe {
        libc::close(fd2);
      }
      panic!("expected shm_open after drop to fail (name should be unlinked)");
    }
    assert_eq!(
      io::Error::last_os_error().raw_os_error(),
      Some(libc::ENOENT),
      "expected shm_open after drop to fail with ENOENT"
    );
  }

  // This test does a best-effort exec-based inheritance check. It is still unit-test scoped (no
  // dependency on the renderer binary), but validates that `dup_to_fd` works in the intended
  // `CommandExt::pre_exec` environment.
  #[cfg(target_os = "linux")]
  #[test]
  fn linux_memfd_inheritance_across_exec_smoke() -> io::Result<()> {
    const CHILD_ENV: &str = "FASTR_SHMEM_TEST_CHILD";
    const SHMEM_FD_ENV: &str = "FASTR_SHMEM_TEST_SHMEM_FD";
    const SHMEM_LEN_ENV: &str = "FASTR_SHMEM_TEST_SHMEM_LEN";
    const TARGET_FD: RawFd = 100;

    if std::env::var_os(CHILD_ENV).is_some() {
      let fd: RawFd = std::env::var(SHMEM_FD_ENV)
        .map_err(|_| io::Error::new(io::ErrorKind::NotFound, "missing SHMEM_FD_ENV"))?
        .parse::<i32>()
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "invalid SHMEM_FD_ENV"))?;
      let len: usize = std::env::var(SHMEM_LEN_ENV)
        .map_err(|_| io::Error::new(io::ErrorKind::NotFound, "missing SHMEM_LEN_ENV"))?
        .parse::<usize>()
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "invalid SHMEM_LEN_ENV"))?;

      let handle = ShmemHandle::LinuxMemfd { fd, len };
      let mut region = ShmemRegion::map(&handle)?;
      assert_eq!(&region.as_slice()[0..4], b"PING");
      region.as_mut_slice()[0..4].copy_from_slice(b"PONG");
      return Ok(());
    }

    let (mut region, handle) = ShmemRegion::create(ShmemBackend::LinuxMemfd, 4096)?;
    region.as_mut_slice()[0..4].copy_from_slice(b"PING");

    let exe = std::env::current_exe()?;
    let mut cmd = std::process::Command::new(exe);
    cmd.env(CHILD_ENV, "1");
    cmd.env(SHMEM_FD_ENV, TARGET_FD.to_string());
    cmd.env(SHMEM_LEN_ENV, handle.len().to_string());
    // Run only this test in the child.
    cmd.arg("linux_memfd_inheritance_across_exec_smoke");

    // SAFETY: pre_exec runs after fork and before exec; the closure must only use
    // async-signal-safe operations (here: `dup`/`dup2`/`close`).
    let handle_for_child = handle.clone();
    unsafe {
      cmd.pre_exec(move || handle_for_child.dup_to_fd(TARGET_FD));
    }

    let status = cmd.status()?;
    assert!(status.success(), "child exited with {status:?}");
    assert_eq!(&region.as_slice()[0..4], b"PONG");

    Ok(())
  }
}
