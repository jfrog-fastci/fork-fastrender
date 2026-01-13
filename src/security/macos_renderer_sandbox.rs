use std::fmt;

/// IPC transport used between the browser (trusted) and renderer (sandboxed).
///
/// This enum exists so we can keep the macOS Seatbelt profile in sync with the
/// OS primitives used by the chosen IPC mechanism.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RendererIpcMechanism {
  /// Anonymous pipes / inherited file descriptors only.
  ///
  /// This is the most "portable" option and is compatible with a strict sandbox
  /// as long as the browser creates the file descriptors before the renderer is
  /// sandboxed.
  PipesOnly,

  /// Pipes + POSIX shared memory (`shm_open`).
  PosixShm,

  /// Filesystem-path Unix domain sockets (`AF_UNIX`, `sockaddr_un`).
  UnixSocket,

  /// Mach ports / bootstrap services (e.g. `ipc-channel`-style transport on macOS).
  MachPort,
}

/// Build the Seatbelt (SBPL) profile string for the renderer process.
///
/// The profile is intentionally structured so IPC-related allowances live behind
/// [`RendererIpcMechanism`]. Future IPC choices should only require extending the
/// enum + match here.
///
/// Notes:
/// - This is only meaningful on macOS; other platforms ignore the returned SBPL.
/// - The profile is applied in tests after the process has started (via
///   `sandbox_init`), which avoids needing to allow filesystem access for the
///   dynamic linker, etc.
#[must_use]
pub fn build_renderer_sbpl(ipc: RendererIpcMechanism) -> String {
  // SBPL is a small S-expression language. Keep the output deterministic and
  // line-based for easy diffing and log inspection.
  let mut sbpl = String::new();
  sbpl.push_str("(version 1)\n");
  sbpl.push_str("(deny default)\n");

  // ---------------------------------------------------------------------------
  // Baseline allowances
  // ---------------------------------------------------------------------------
  //
  // The eventual renderer sandbox will need a carefully curated baseline.
  // For now we keep the baseline small and focused on IPC regression tests.
  //
  // - `process*` prevents unexpected failures from libc/Rust querying process
  //   metadata.
  // - `file-read*`/`file-write*` keeps stdio and basic runtime behavior working
  //   in minimal probes.
  sbpl.push_str("(allow process*)\n");
  sbpl.push_str("(allow file-read*)\n");
  sbpl.push_str("(allow file-write*)\n");

  // ---------------------------------------------------------------------------
  // IPC allowances (toggled by RendererIpcMechanism)
  // ---------------------------------------------------------------------------
  match ipc {
    RendererIpcMechanism::PipesOnly => {
      // Pipes/inherited FDs: no additional SBPL operations are required beyond
      // allowing basic file I/O on existing descriptors.
    }
    RendererIpcMechanism::PosixShm => {
      // POSIX shared memory objects created via `shm_open`.
      //
      // Seatbelt gates `shm_open` under the `ipc-posix-shm` operation.
      sbpl.push_str("(allow ipc-posix-shm)\n");
    }
    RendererIpcMechanism::UnixSocket => {
      // Unix domain sockets are mediated by the `network-*` operations; allow
      // outbound connects to AF_UNIX sockets.
      //
      // Note: this is intentionally narrow (unix-socket only) and does not
      // enable TCP/UDP.
      sbpl.push_str("(allow network-outbound (remote unix-socket))\n");
    }
    RendererIpcMechanism::MachPort => {
      // Mach bootstrap lookups are gated by `mach-lookup`.
      //
      // Most real-world profiles scope this to a small allowlist of service
      // names via `(global-name "...")`. We keep this broad for now, and will
      // tighten once the concrete IPC design is finalized.
      sbpl.push_str("(allow mach-lookup)\n");
    }
  }

  sbpl
}

// =============================================================================
// Applying sandbox profiles
// =============================================================================

/// Error applying the macOS sandbox profile.
#[derive(Debug)]
pub enum MacosSandboxError {
  /// The current platform does not support Seatbelt sandboxing.
  UnsupportedPlatform,
  /// `sandbox_init` returned an error.
  InitFailed { message: String },
}

impl fmt::Display for MacosSandboxError {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    match self {
      MacosSandboxError::UnsupportedPlatform => write!(f, "unsupported platform"),
      MacosSandboxError::InitFailed { message } => write!(f, "sandbox_init failed: {message}"),
    }
  }
}

impl std::error::Error for MacosSandboxError {}

/// Apply a Seatbelt sandbox profile (SBPL) to the current process.
///
/// This is irreversible for the lifetime of the process.
pub fn apply_sbpl(profile: &str) -> Result<(), MacosSandboxError> {
  apply_sbpl_impl(profile)
}

#[cfg(target_os = "macos")]
fn apply_sbpl_impl(profile: &str) -> Result<(), MacosSandboxError> {
  use std::ffi::CString;
  use std::ptr;

  // `sandbox_init` signature:
  //   int sandbox_init(const char *profile, uint64_t flags, char **errorbuf);
  extern "C" {
    fn sandbox_init(
      profile: *const std::os::raw::c_char,
      flags: u64,
      errorbuf: *mut *mut std::os::raw::c_char,
    ) -> std::os::raw::c_int;
    fn sandbox_free_error(errorbuf: *mut std::os::raw::c_char);
  }

  let profile_c = CString::new(profile).expect("SBPL profile contains interior NUL byte"); // fastrender-allow-unwrap
  let mut error_buf: *mut std::os::raw::c_char = ptr::null_mut();

  // Flags = 0 => profile is an SBPL string, not a named builtin profile.
  let rc = unsafe { sandbox_init(profile_c.as_ptr(), 0, &mut error_buf) };
  if rc == 0 {
    return Ok(());
  }

  let message = if error_buf.is_null() {
    "<no error message>".to_string()
  } else {
    // Safety: `error_buf` is a NUL-terminated C string allocated by libsandbox.
    let msg = unsafe { std::ffi::CStr::from_ptr(error_buf) }
      .to_string_lossy()
      .into_owned();
    unsafe { sandbox_free_error(error_buf) };
    msg
  };

  Err(MacosSandboxError::InitFailed { message })
}

#[cfg(not(target_os = "macos"))]
fn apply_sbpl_impl(_profile: &str) -> Result<(), MacosSandboxError> {
  Err(MacosSandboxError::UnsupportedPlatform)
}
