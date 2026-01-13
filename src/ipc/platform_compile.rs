#[test]
fn platform_compile_smoke() {
  // This test exists purely to keep platform-specific IPC code paths compiling on each OS in CI.
  // Avoid allocating OS resources (no sockets, memfd, shm_open, etc.).

  #[cfg(target_os = "linux")]
  {
    use crate::ipc::shm::{OwnedShm, ReceivedShm, ShmError};
    use fastrender_shmem::{ShmemBackend, ShmemHandle};
    use std::os::unix::io::RawFd;

    // Ensure the Linux memfd + mmap backend still exists and is wired.
    let _ = std::mem::size_of::<OwnedShm>();
    let _ = std::mem::size_of::<ReceivedShm>();

    // Touch Linux-only error variants so rename/removal is caught.
    let _ = ShmError::MemfdCreateFailed {
      source: std::io::Error::new(std::io::ErrorKind::Other, "compile-smoke"),
    };
    let _ = ShmError::TruncateFailed {
      size: 1,
      source: std::io::Error::new(std::io::ErrorKind::Other, "compile-smoke"),
    };
    let _ = ShmError::MmapFailed {
      size: 1,
      source: std::io::Error::new(std::io::ErrorKind::Other, "compile-smoke"),
    };
    let _ = ShmError::StatFailed {
      source: std::io::Error::new(std::io::ErrorKind::Other, "compile-smoke"),
    };
    let _ = ShmError::SizeMismatch {
      expected: 1,
      actual: 1,
    };
    let _ = ShmError::SizeExceedsMax { actual: 1, max: 1 };
    let _ = ShmError::SealFailed {
      source: std::io::Error::new(std::io::ErrorKind::Other, "compile-smoke"),
    };

    // Ensure the fastrender-shmem Linux memfd backend API remains available.
    let _ = ShmemBackend::LinuxMemfd;
    let _ = ShmemHandle::LinuxMemfd { fd: 3, len: 1 };

    // Reference Linux-only handle helpers without executing them.
    let _ = fastrender_shmem::ShmemHandle::dup_to_fd as fn(&ShmemHandle, RawFd) -> std::io::Result<()>;
    let _ = fastrender_shmem::ShmemHandle::clear_cloexec as fn(&ShmemHandle) -> std::io::Result<()>;
  }

  #[cfg(target_os = "macos")]
  {
    use crate::security::macos_renderer_sandbox::{build_renderer_sbpl, apply_sbpl, MacosSandboxError, RendererIpcMechanism};
    use fastrender_shmem::{ShmemBackend, ShmemHandle, ShmemRegion};

    // Ensure the Seatbelt IPC selector stays in sync with shared-memory transport choices.
    let _ = RendererIpcMechanism::PosixShm;
    let _ = build_renderer_sbpl as fn(RendererIpcMechanism) -> String;
    let _ = apply_sbpl as fn(&str) -> Result<(), MacosSandboxError>;

    // Ensure POSIX shared-memory naming/path handling still compiles on macOS.
    let _ = ShmemBackend::PosixShm;
    let _ = ShmemHandle::PosixShm {
      id: String::new(),
      len: 1,
    };
    let _ = fastrender_shmem::generate_shmem_id as fn() -> String;
    let _ = ShmemRegion::create
      as fn(ShmemBackend, usize) -> std::io::Result<(ShmemRegion, ShmemHandle)>;
    let _ = ShmemRegion::map as fn(&ShmemHandle) -> std::io::Result<ShmemRegion>;
  }

  #[cfg(windows)]
  {
    use win_sandbox::{Job, RendererSandbox, RestrictedToken};

    // Smoke-reference Windows sandbox/IPC security primitives (DACL handling lives underneath).
    let _ = std::mem::size_of::<Job>();
    let _ = std::mem::size_of::<RendererSandbox>();
    let _ = std::mem::size_of::<RestrictedToken>();

    // Ensure the Win32 DACL constant remains reachable (used by sandbox/IPC setup).
    let _ = windows_sys::Win32::Security::DACL_SECURITY_INFORMATION;
  }
}

