use crate::{OwnedSid, Result, WinSandboxError};

use std::ffi::c_void;

type HRESULT = i32;

const FACILITY_WIN32: u32 = 7;
const ERROR_ALREADY_EXISTS: u32 = 183;

const fn hresult_from_win32(error: u32) -> HRESULT {
  if error == 0 {
    0
  } else {
    (0x8000_0000u32 | (FACILITY_WIN32 << 16) | (error & 0xFFFF)) as HRESULT
  }
}

fn wide_null(s: &str) -> Vec<u16> {
  s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// An AppContainer identity (profile + SID).
///
/// Note: while an AppContainer SID can be deterministically derived from its name
/// (see [`derive_appcontainer_sid`]), in practice many Windows APIs that create an
/// AppContainer token/process require the profile to be registered with the system.
///
/// For that reason [`AppContainerProfile::ensure`] always calls
/// `CreateAppContainerProfile` and treats `ERROR_ALREADY_EXISTS` as success.
#[derive(Debug)]
pub struct AppContainerProfile {
  sid: OwnedSid,
}

impl AppContainerProfile {
  /// Ensures an AppContainer profile exists for `name` and returns its SID.
  ///
  /// This operation is intentionally idempotent: if the profile already exists,
  /// `ERROR_ALREADY_EXISTS` is treated as success.
  pub fn ensure(name: &str, display_name: &str, description: &str) -> Result<Self> {
    let name_w = wide_null(name);
    let display_name_w = wide_null(display_name);
    let description_w = wide_null(description);

    // SAFETY: Win32 FFI call.
    let mut sid: *mut c_void = std::ptr::null_mut();
    let hr = unsafe {
      CreateAppContainerProfile(
        name_w.as_ptr(),
        display_name_w.as_ptr(),
        description_w.as_ptr(),
        std::ptr::null(),
        0,
        &mut sid,
      )
    };

    if hr == hresult_from_win32(ERROR_ALREADY_EXISTS) {
      return Ok(Self {
        sid: derive_appcontainer_sid(name)?,
      });
    }

    if hr < 0 {
      return Err(WinSandboxError::from_hresult("CreateAppContainerProfile", hr));
    }

    if sid.is_null() {
      return Err(WinSandboxError::NullPointer {
        func: "CreateAppContainerProfile",
      });
    }

    Ok(Self {
      sid: OwnedSid::from_free_sid(sid as _),
    })
  }

  pub fn sid(&self) -> &OwnedSid {
    &self.sid
  }
}

/// Derives an AppContainer SID from its name.
///
/// This does **not** ensure the corresponding profile exists. Prefer
/// [`AppContainerProfile::ensure`] when preparing to spawn a process into an
/// AppContainer.
pub fn derive_appcontainer_sid(name: &str) -> Result<OwnedSid> {
  let name_w = wide_null(name);

  // SAFETY: Win32 FFI call.
  let mut sid: *mut c_void = std::ptr::null_mut();
  let hr = unsafe { DeriveAppContainerSidFromAppContainerName(name_w.as_ptr(), &mut sid) };
  if hr < 0 {
    return Err(WinSandboxError::from_hresult(
      "DeriveAppContainerSidFromAppContainerName",
      hr,
    ));
  }

  if sid.is_null() {
    return Err(WinSandboxError::NullPointer {
      func: "DeriveAppContainerSidFromAppContainerName",
    });
  }

  Ok(OwnedSid::from_free_sid(sid as _))
}

#[repr(C)]
struct SID_AND_ATTRIBUTES {
  sid: *mut c_void,
  attributes: u32,
}

#[link(name = "userenv")]
extern "system" {
  fn CreateAppContainerProfile(
    pszAppContainerName: *const u16,
    pszDisplayName: *const u16,
    pszDescription: *const u16,
    pCapabilities: *const SID_AND_ATTRIBUTES,
    dwCapabilityCount: u32,
    ppSidAppContainerSid: *mut *mut c_void,
  ) -> HRESULT;

  fn DeriveAppContainerSidFromAppContainerName(
    pszAppContainerName: *const u16,
    ppsidAppContainerSid: *mut *mut c_void,
  ) -> HRESULT;
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::WinSandboxError;
  use std::ffi::c_void;

  type BOOL = i32;
  type HANDLE = isize;
  type DWORD = u32;

  const TOKEN_QUERY: DWORD = 0x0008;
  const TOKEN_IS_APPCONTAINER: DWORD = 29;

  #[link(name = "advapi32")]
  extern "system" {
    fn OpenProcessToken(ProcessHandle: HANDLE, DesiredAccess: DWORD, TokenHandle: *mut HANDLE)
      -> BOOL;

    fn GetTokenInformation(
      TokenHandle: HANDLE,
      TokenInformationClass: DWORD,
      TokenInformation: *mut c_void,
      TokenInformationLength: DWORD,
      ReturnLength: *mut DWORD,
    ) -> BOOL;
  }

  #[link(name = "kernel32")]
  extern "system" {
    fn CloseHandle(hObject: HANDLE) -> BOOL;
    fn GetCurrentProcess() -> HANDLE;
    fn GetLastError() -> DWORD;
  }

  const INVALID_HANDLE_VALUE: HANDLE = -1isize;

  struct OwnedHandle(HANDLE);

  impl Drop for OwnedHandle {
    fn drop(&mut self) {
      unsafe {
        if self.0 != 0 && self.0 != INVALID_HANDLE_VALUE {
          CloseHandle(self.0);
        }
      }
    }
  }

  fn current_process_is_appcontainer() -> Result<bool> {
    unsafe {
      let mut token: HANDLE = 0;
      if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
        return Err(WinSandboxError::from_code("OpenProcessToken", GetLastError()));
      }
      let token = OwnedHandle(token);

      let mut is_appcontainer: DWORD = 0;
      let mut return_len: DWORD = 0;
      if GetTokenInformation(
        token.0,
        TOKEN_IS_APPCONTAINER,
        (&mut is_appcontainer as *mut DWORD).cast::<c_void>(),
        std::mem::size_of::<DWORD>() as DWORD,
        &mut return_len,
      ) == 0
      {
        return Err(WinSandboxError::from_code(
          "GetTokenInformation(TokenIsAppContainer)",
          GetLastError(),
        ));
      }

      Ok(is_appcontainer != 0)
    }
  }

  #[test]
  fn ensure_appcontainer_profile_is_idempotent() -> Result<()> {
    assert!(!current_process_is_appcontainer()?);

    let _profile = AppContainerProfile::ensure(
      "FastRender.Renderer",
      "FastRender Renderer",
      "FastRender renderer AppContainer profile",
    )?;
    let _profile2 = AppContainerProfile::ensure(
      "FastRender.Renderer",
      "FastRender Renderer",
      "FastRender renderer AppContainer profile",
    )?;

    // Creating a profile should not change the current process token.
    assert!(!current_process_is_appcontainer()?);
    Ok(())
  }
}
