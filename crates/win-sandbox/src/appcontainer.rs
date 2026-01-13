use crate::{OwnedSid, Result, WinSandboxError};

use std::ffi::c_void;
use std::sync::OnceLock;

use windows_sys::Win32::Foundation::{FreeLibrary, HMODULE, ERROR_PROC_NOT_FOUND};
use windows_sys::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};

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

fn wide_null(arg: &'static str, s: &str) -> Result<Vec<u16>> {
  if s.chars().any(|c| c == '\0') {
    return Err(WinSandboxError::InteriorNul { arg });
  }
  Ok(s.encode_utf16().chain(std::iter::once(0)).collect())
}

// -----------------------------------------------------------------------------
// userenv.dll dynamic loader
// -----------------------------------------------------------------------------
//
// AppContainer profile management APIs are not present on older Windows releases. If we link them
// directly, the loader can fail the entire process at startup due to a missing entrypoint. Resolve
// the functions dynamically so callers can gracefully fall back to a weaker sandbox.

#[derive(Debug)]
struct UserenvApis {
  // Keep the backing module loaded for the lifetime of the process. The function pointers below are
  // only valid while `userenv.dll` remains loaded.
  _module: HMODULE,
  create_app_container_profile: CreateAppContainerProfileFn,
  derive_app_container_sid_from_app_container_name: DeriveAppContainerSidFromAppContainerNameFn,
}

// `HMODULE` is an opaque handle. It is safe to share this struct across threads because we never
// dereference the module handle; it's only kept alive to maintain the DLL reference count, and the
// function pointers are immutable.
unsafe impl Send for UserenvApis {}
unsafe impl Sync for UserenvApis {}
#[derive(Debug)]
enum UserenvAvailability {
  Supported(UserenvApis),
  Unsupported(WinSandboxError),
}

fn userenv_apis() -> Result<&'static UserenvApis> {
  static AVAILABILITY: OnceLock<UserenvAvailability> = OnceLock::new();
  match AVAILABILITY.get_or_init(|| unsafe { UserenvAvailability::load() }) {
    UserenvAvailability::Supported(apis) => Ok(apis),
    UserenvAvailability::Unsupported(err) => Err(err.clone()),
  }
}

impl UserenvAvailability {
  unsafe fn load() -> Self {
    match UserenvApis::load() {
      Ok(apis) => Self::Supported(apis),
      Err(err) => Self::Unsupported(err),
    }
  }
}

impl UserenvApis {
  unsafe fn load() -> Result<Self> {
    let dll_w = wide_null("dll_name", "userenv.dll")?;
    let module = LoadLibraryW(dll_w.as_ptr());
    if module == 0 as HMODULE {
      return Err(WinSandboxError::last("LoadLibraryW(userenv.dll)"));
    }

    let create_app_container_profile = match get_proc::<CreateAppContainerProfileFn>(
      module,
      b"CreateAppContainerProfile\0",
      "GetProcAddress(CreateAppContainerProfile)",
    ) {
      Ok(f) => f,
      Err(err) => {
        let _ = FreeLibrary(module);
        return Err(err);
      }
    };

    let derive_app_container_sid_from_app_container_name =
      match get_proc::<DeriveAppContainerSidFromAppContainerNameFn>(
        module,
        b"DeriveAppContainerSidFromAppContainerName\0",
        "GetProcAddress(DeriveAppContainerSidFromAppContainerName)",
      ) {
        Ok(f) => f,
        Err(err) => {
          let _ = FreeLibrary(module);
          return Err(err);
        }
      };

    Ok(Self {
      _module: module,
      create_app_container_profile,
      derive_app_container_sid_from_app_container_name,
    })
  }
}

unsafe fn get_proc<T>(module: HMODULE, symbol: &'static [u8], func: &'static str) -> Result<T> {
  // SAFETY: `symbol` is a pointer to a null-terminated ASCII string.
  //
  // Reset last-error to avoid returning a stale value if `GetProcAddress` does
  // not set it in some edge case (e.g. older/stripped builds). The documented
  // error for a missing symbol is `ERROR_PROC_NOT_FOUND` (127).
  windows_sys::Win32::Foundation::SetLastError(0);
  let proc = GetProcAddress(module, symbol.as_ptr());
  match proc {
    Some(proc) => {
      // SAFETY: The caller is responsible for ensuring `T` matches the actual exported symbol's
      // signature.
      Ok(std::mem::transmute_copy(&proc))
    }
    None => {
      let err = windows_sys::Win32::Foundation::GetLastError();
      let err = if err == 0 { ERROR_PROC_NOT_FOUND } else { err };
      Err(WinSandboxError::from_code(func, err))
    }
  }
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
    let apis = userenv_apis()?;

    let name_w = wide_null("name", name)?;
    let display_name_w = wide_null("display_name", display_name)?;
    let description_w = wide_null("description", description)?;

    // SAFETY: Win32 FFI call.
    let mut sid: *mut c_void = std::ptr::null_mut();
    let hr = unsafe {
      (apis.create_app_container_profile)(
        name_w.as_ptr(),
        display_name_w.as_ptr(),
        description_w.as_ptr(),
        std::ptr::null(),
        0,
        &mut sid,
      )
    };

    if hr == hresult_from_win32(ERROR_ALREADY_EXISTS) {
      if !sid.is_null() {
        // Some Windows builds may still return the SID on `ERROR_ALREADY_EXISTS`.
        return Ok(Self {
          sid: OwnedSid::from_free_sid(sid as _),
        });
      }
      return Ok(Self {
        sid: derive_appcontainer_sid(name)?,
      });
    }

    if hr < 0 {
      if !sid.is_null() {
        // SAFETY: AppContainer SIDs are freed with FreeSid.
        unsafe {
          windows_sys::Win32::Security::FreeSid(sid as _);
        }
      }
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
  let apis = userenv_apis()?;
  let name_w = wide_null("name", name)?;

  // SAFETY: Win32 FFI call.
  let mut sid: *mut c_void = std::ptr::null_mut();
  let hr =
    unsafe { (apis.derive_app_container_sid_from_app_container_name)(name_w.as_ptr(), &mut sid) };
  if hr < 0 {
    if !sid.is_null() {
      // SAFETY: AppContainer SIDs are freed with FreeSid.
      unsafe {
        windows_sys::Win32::Security::FreeSid(sid as _);
      }
    }
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
struct SidAndAttributes {
  sid: *mut c_void,
  attributes: u32,
}

type CreateAppContainerProfileFn = unsafe extern "system" fn(
  *const u16,
  *const u16,
  *const u16,
  *const SidAndAttributes,
  u32,
  *mut *mut c_void,
) -> HRESULT;

type DeriveAppContainerSidFromAppContainerNameFn =
  unsafe extern "system" fn(*const u16, *mut *mut c_void) -> HRESULT;
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

    let _profile = match AppContainerProfile::ensure(
      "FastRender.Renderer",
      "FastRender Renderer",
      "FastRender renderer AppContainer profile",
    ) {
      Ok(profile) => profile,
      Err(WinSandboxError::Win32 { code, .. })
        if code == windows_sys::Win32::Foundation::ERROR_PROC_NOT_FOUND =>
      {
        eprintln!("AppContainer APIs not available on this host; skipping test");
        return Ok(());
      }
      Err(err) => return Err(err),
    };
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
