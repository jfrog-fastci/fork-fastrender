use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SandboxSupport {
  /// AppContainer + nested job objects are available.
  Full,
  /// Job objects are usable, but AppContainer primitives are missing.
  NoAppContainer,
  /// AppContainer primitives are available, but nested job objects are missing.
  NoNestedJob,
  /// Neither AppContainer nor nested job objects appear to be available.
  Unsupported,
}

impl SandboxSupport {
  #[must_use]
  pub fn detect() -> Self {
    let appcontainer = is_appcontainer_supported();
    let nested_job = is_nested_job_supported();
    match (appcontainer, nested_job) {
      (true, true) => SandboxSupport::Full,
      (false, true) => SandboxSupport::NoAppContainer,
      (true, false) => SandboxSupport::NoNestedJob,
      (false, false) => SandboxSupport::Unsupported,
    }
  }
}

impl fmt::Display for SandboxSupport {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    match self {
      SandboxSupport::Full => write!(f, "full support (AppContainer + nested jobs)"),
      SandboxSupport::NoAppContainer => write!(f, "missing AppContainer support"),
      SandboxSupport::NoNestedJob => write!(f, "missing nested job support"),
      SandboxSupport::Unsupported => write!(f, "unsupported (missing AppContainer + nested jobs)"),
    }
  }
}

#[cfg(not(windows))]
pub fn is_appcontainer_supported() -> bool {
  false
}

#[cfg(not(windows))]
pub fn is_nested_job_supported() -> bool {
  false
}

#[cfg(windows)]
pub fn is_appcontainer_supported() -> bool {
  use std::sync::OnceLock;

  static SUPPORTED: OnceLock<bool> = OnceLock::new();
  *SUPPORTED.get_or_init(|| unsafe { is_appcontainer_supported_impl() })
}

#[cfg(windows)]
pub fn is_nested_job_supported() -> bool {
  use std::sync::OnceLock;

  static SUPPORTED: OnceLock<bool> = OnceLock::new();
  *SUPPORTED.get_or_init(|| unsafe { is_nested_job_supported_impl() })
}

#[cfg(windows)]
unsafe fn is_appcontainer_supported_impl() -> bool {
  use windows_sys::Win32::Foundation::HMODULE;
  use windows_sys::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress, LoadLibraryW};

  // `userenv.dll` provides the AppContainer profile APIs on Windows 8+.
  let userenv: Vec<u16> = "userenv.dll\0".encode_utf16().collect();
  let mut module: HMODULE = GetModuleHandleW(userenv.as_ptr());
  if module.is_null() {
    module = LoadLibraryW(userenv.as_ptr());
  }
  if module.is_null() {
    return false;
  }

  let create_profile = GetProcAddress(module, b"CreateAppContainerProfile\0".as_ptr());
  let derive_sid = GetProcAddress(
    module,
    b"DeriveAppContainerSidFromAppContainerName\0".as_ptr(),
  );
  create_profile.is_some() && derive_sid.is_some()
}

#[cfg(windows)]
unsafe fn is_nested_job_supported_impl() -> bool {
  use windows_sys::Win32::System::JobObjects::IsProcessInJob;
  use windows_sys::Win32::System::Threading::GetCurrentProcess;

  // If the broker is *not* in a job already, we can still sandbox the renderer in a job even on
  // downlevel OS versions. Nested jobs are only required when the current process is already
  // running under a job object (common under CI runners, shells, and some host sandboxes).
  let mut in_job: i32 = 0;
  let ok = IsProcessInJob(GetCurrentProcess(), std::ptr::null_mut(), &mut in_job);
  if ok == 0 {
    // If we cannot query job membership, be conservative and treat nested job support as missing.
    return false;
  }
  if in_job == 0 {
    return true;
  }

  is_windows_8_or_greater()
}

#[cfg(windows)]
fn is_windows_8_or_greater() -> bool {
  // Windows 8 is NT 6.2.
  const WIN8_MAJOR: u32 = 6;
  const WIN8_MINOR: u32 = 2;

  let (major, minor) = match rtl_get_version() {
    Some(v) => v,
    None => return false,
  };
  major > WIN8_MAJOR || (major == WIN8_MAJOR && minor >= WIN8_MINOR)
}

#[cfg(windows)]
fn rtl_get_version() -> Option<(u32, u32)> {
  // `RtlGetVersion` is the recommended way to get an accurate Windows version without requiring a
  // manifest (unlike `GetVersionExW`).
  #[repr(C)]
  struct RTL_OSVERSIONINFOW {
    dw_os_version_info_size: u32,
    dw_major_version: u32,
    dw_minor_version: u32,
    dw_build_number: u32,
    dw_platform_id: u32,
    sz_csd_version: [u16; 128],
  }

  #[link(name = "ntdll")]
  extern "system" {
    fn RtlGetVersion(version_information: *mut RTL_OSVERSIONINFOW) -> i32; // NTSTATUS
  }

  // SAFETY: We pass a properly-sized struct to `RtlGetVersion`, which writes to it.
  unsafe {
    let mut info: RTL_OSVERSIONINFOW = std::mem::zeroed();
    info.dw_os_version_info_size = std::mem::size_of::<RTL_OSVERSIONINFOW>() as u32;
    let status = RtlGetVersion(&mut info);
    if status != 0 {
      return None;
    }
    Some((info.dw_major_version, info.dw_minor_version))
  }
}
