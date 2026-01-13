#![cfg(windows)]

use win_sandbox::{AppContainerProfile, SandboxSupport, WinSandboxError};

const APPCONTAINER_NAME: &str = "FastRender.Renderer";
const APPCONTAINER_DISPLAY_NAME: &str = "FastRender Renderer";
const APPCONTAINER_DESCRIPTION: &str = "FastRender renderer AppContainer profile";

fn hresult_from_win32_code(hresult: u32) -> Option<u32> {
  // `HRESULT_FROM_WIN32` encodes the original Win32 error code in the low 16 bits with facility 7.
  const FACILITY_WIN32_MASK: u32 = 0xFFFF_0000;
  const FACILITY_WIN32_PREFIX: u32 = 0x8007_0000;
  if (hresult & FACILITY_WIN32_MASK) == FACILITY_WIN32_PREFIX {
    Some(hresult & 0xFFFF)
  } else {
    None
  }
}

fn win32_code_from_error(err: &WinSandboxError) -> Option<u32> {
  match err {
    WinSandboxError::Win32 { code, .. } => Some(*code),
    WinSandboxError::HResult { hresult, .. } => hresult_from_win32_code(*hresult),
    _ => None,
  }
}

fn should_skip_appcontainer_error(err: &WinSandboxError) -> bool {
  use windows_sys::Win32::Foundation::{
    ERROR_ACCESS_DENIED, ERROR_NOT_SUPPORTED, ERROR_PROC_NOT_FOUND,
  };

  // Returned when software restriction policies / group policy blocks an operation.
  // This is environment policy, not a win-sandbox regression.
  const ERROR_ACCESS_DISABLED_BY_POLICY: u32 = 1260;
  const ERROR_PRIVILEGE_NOT_HELD: u32 = 1314;

  match win32_code_from_error(err) {
    Some(code) => matches!(
      code,
      ERROR_ACCESS_DENIED
        | ERROR_ACCESS_DISABLED_BY_POLICY
        | ERROR_PRIVILEGE_NOT_HELD
        | ERROR_NOT_SUPPORTED
        | ERROR_PROC_NOT_FOUND
    ),
    None => false,
  }
}

/// Returns `true` when AppContainer profile APIs appear usable on this host.
///
/// GitHub-hosted Windows runners generally support AppContainer, but some hardened environments may
/// expose the APIs while still denying profile creation. In that case, AppContainer-dependent tests
/// should skip with a clear message rather than failing the entire suite.
pub(crate) fn require_appcontainer_profile(test_name: &str) -> bool {
  if !win_sandbox::is_appcontainer_supported() {
    eprintln!("skipping {test_name}: AppContainer APIs are unavailable on this OS");
    return false;
  }

  match AppContainerProfile::ensure(
    APPCONTAINER_NAME,
    APPCONTAINER_DISPLAY_NAME,
    APPCONTAINER_DESCRIPTION,
  ) {
    Ok(profile) => {
      if !profile.is_enabled() {
        eprintln!("skipping {test_name}: AppContainer profile is disabled");
        return false;
      }
      true
    }
    Err(err) if should_skip_appcontainer_error(&err) => {
      eprintln!("skipping {test_name}: AppContainer profile could not be ensured ({err})");
      false
    }
    Err(err) => panic!(
      "{test_name}: AppContainer profile ensure failed unexpectedly: {err}\n\
This likely indicates a regression in the AppContainer support code, not missing OS support."
    ),
  }
}

pub(crate) fn require_full_sandbox_support(test_name: &str) -> bool {
  let support = SandboxSupport::detect();
  if support != SandboxSupport::Full {
    eprintln!("skipping {test_name}: full sandbox support unavailable ({support})");
    return false;
  }

  require_appcontainer_profile(test_name)
}
