use std::ffi::c_void;
use std::io;
use std::mem;
use std::sync::OnceLock;

#[derive(Debug)]
pub struct AppContainerApis {
  /// Keep the backing module loaded for the lifetime of the process.
  ///
  /// The function pointers below are only valid while `userenv.dll` remains loaded.
  _userenv: HMODULE,
  pub(crate) create_app_container_profile: CreateAppContainerProfileFn,
  pub(crate) derive_app_container_sid_from_app_container_name: DeriveAppContainerSidFromAppContainerNameFn,
  pub(crate) delete_app_container_profile: DeleteAppContainerProfileFn,
}

#[derive(Debug, thiserror::Error)]
pub enum AppContainerApiLoadError {
  #[error("failed to load userenv.dll (required for AppContainer sandboxing): {source}")]
  LoadUserenvFailed {
    #[source]
    source: io::Error,
  },

  #[error("userenv.dll is missing required AppContainer symbol `{symbol}`: {source}")]
  MissingSymbol {
    symbol: &'static str,
    #[source]
    source: io::Error,
  },
}

#[derive(Debug)]
enum AppContainerApiAvailability {
  Supported(AppContainerApis),
  Unsupported(AppContainerApiLoadError),
}

/// Returns dynamically-resolved AppContainer APIs on supported Windows versions.
///
/// On older Windows releases that do not ship AppContainer, the relevant symbols are absent from
/// `userenv.dll`. To keep the binary loadable on those OS versions, we *must not* link these APIs
/// directly. Instead, we resolve them at runtime and treat missing symbols as "AppContainer is not
/// supported" so callers can fall back to a less strict sandbox (e.g. restricted token).
pub fn appcontainer_apis(
) -> Result<&'static AppContainerApis, &'static AppContainerApiLoadError> {
  static AVAILABILITY: OnceLock<AppContainerApiAvailability> = OnceLock::new();
  match AVAILABILITY.get_or_init(|| unsafe { AppContainerApiAvailability::load() }) {
    AppContainerApiAvailability::Supported(apis) => Ok(apis),
    AppContainerApiAvailability::Unsupported(err) => Err(err),
  }
}

impl AppContainerApiAvailability {
  unsafe fn load() -> Self {
    match AppContainerApis::load() {
      Ok(apis) => Self::Supported(apis),
      Err(err) => Self::Unsupported(err),
    }
  }
}

impl AppContainerApis {
  unsafe fn load() -> Result<Self, AppContainerApiLoadError> {
    let userenv = load_userenv()?;

    // Keep the symbol names in `&'static str` form so error messages are readable.
    let create_app_container_profile = match get_userenv_proc::<CreateAppContainerProfileFn>(
      userenv,
      "CreateAppContainerProfile",
      b"CreateAppContainerProfile\0",
    ) {
      Ok(f) => f,
      Err(err) => {
        let _ = FreeLibrary(userenv);
        return Err(err);
      }
    };

    let derive_app_container_sid_from_app_container_name =
      match get_userenv_proc::<DeriveAppContainerSidFromAppContainerNameFn>(
        userenv,
        "DeriveAppContainerSidFromAppContainerName",
        b"DeriveAppContainerSidFromAppContainerName\0",
      ) {
        Ok(f) => f,
        Err(err) => {
          let _ = FreeLibrary(userenv);
          return Err(err);
        }
      };

    let delete_app_container_profile = match get_userenv_proc::<DeleteAppContainerProfileFn>(
      userenv,
      "DeleteAppContainerProfile",
      b"DeleteAppContainerProfile\0",
    ) {
      Ok(f) => f,
      Err(err) => {
        let _ = FreeLibrary(userenv);
        return Err(err);
      }
    };

    Ok(Self {
      _userenv: userenv,
      create_app_container_profile,
      derive_app_container_sid_from_app_container_name,
      delete_app_container_profile,
    })
  }
}

// -----------------------------------------------------------------------------
// Windows FFI
// -----------------------------------------------------------------------------

type HMODULE = *mut c_void;

#[repr(C)]
pub(crate) struct SidAndAttributes {
  pub(crate) sid: *mut c_void,
  pub(crate) attributes: u32,
}

type HRESULT = i32;

// See: https://learn.microsoft.com/en-us/windows/win32/api/userenv/nf-userenv-createappcontainerprofile
pub(crate) type CreateAppContainerProfileFn = unsafe extern "system" fn(
  app_container_name: *const u16,
  display_name: *const u16,
  description: *const u16,
  capabilities: *const SidAndAttributes,
  capability_count: u32,
  app_container_sid: *mut *mut c_void,
) -> HRESULT;

// See: https://learn.microsoft.com/en-us/windows/win32/api/userenv/nf-userenv-deriveappcontainersidfromappcontainername
pub(crate) type DeriveAppContainerSidFromAppContainerNameFn =
  unsafe extern "system" fn(app_container_name: *const u16, app_container_sid: *mut *mut c_void)
    -> HRESULT;

// See: https://learn.microsoft.com/en-us/windows/win32/api/userenv/nf-userenv-deleteappcontainerprofile
pub(crate) type DeleteAppContainerProfileFn =
  unsafe extern "system" fn(app_container_name: *const u16) -> HRESULT;

#[link(name = "kernel32")]
extern "system" {
  fn LoadLibraryExW(name: *const u16, hfile: *mut c_void, flags: u32) -> HMODULE;
  fn GetProcAddress(module: HMODULE, proc_name: *const i8) -> *mut c_void;
  fn FreeLibrary(module: HMODULE) -> i32;
  fn GetLastError() -> u32;
  fn SetLastError(dwErrCode: u32);
}

// Force DLL resolution from `%SystemRoot%\\System32` to avoid search-order hijacking.
//
// Value is stable ABI: https://learn.microsoft.com/en-us/windows/win32/api/libloaderapi/nf-libloaderapi-loadlibraryexw
const LOAD_LIBRARY_SEARCH_SYSTEM32: u32 = 0x0000_0800;
const ERROR_PROC_NOT_FOUND: u32 = 127;

unsafe fn load_userenv() -> Result<HMODULE, AppContainerApiLoadError> {
  let wide = "userenv.dll"
    .encode_utf16()
    .chain(std::iter::once(0))
    .collect::<Vec<u16>>();
  let module = LoadLibraryExW(wide.as_ptr(), std::ptr::null_mut(), LOAD_LIBRARY_SEARCH_SYSTEM32);
  if module.is_null() {
    return Err(AppContainerApiLoadError::LoadUserenvFailed {
      source: io::Error::last_os_error(),
    });
  }
  Ok(module)
}

unsafe fn get_userenv_proc<T>(
  module: HMODULE,
  symbol: &'static str,
  symbol_bytes_with_nul: &'static [u8],
) -> Result<T, AppContainerApiLoadError> {
  // Avoid returning a stale `GetLastError()` if `GetProcAddress` doesn't set it
  // for some unexpected reason. The expected error for missing exports is
  // `ERROR_PROC_NOT_FOUND` (127).
  SetLastError(0);
  let proc = GetProcAddress(module, symbol_bytes_with_nul.as_ptr() as *const i8);
  if proc.is_null() {
    let mut err = GetLastError();
    if err == 0 {
      err = ERROR_PROC_NOT_FOUND;
    }
    return Err(AppContainerApiLoadError::MissingSymbol {
      symbol,
      source: io::Error::from_raw_os_error(err as i32),
    });
  }

  // SAFETY: We checked that `proc` is non-null; the caller is responsible for ensuring `T`
  // matches the actual exported symbol's signature.
  Ok(mem::transmute_copy::<*mut c_void, T>(&proc))
}

#[cfg(test)]
mod tests {
  use super::*;

  /// On Windows 10/11, `userenv.dll` should export the AppContainer profile management APIs.
  ///
  /// Marked ignored because CI does not guarantee a Windows host, and older Windows versions are
  /// expected to *not* support AppContainer (in which case this test would fail).
  #[test]
  #[ignore]
  fn userenv_appcontainer_symbols_resolve_on_modern_windows() {
    let apis = appcontainer_apis().expect("AppContainer APIs should be available on this OS");
    // Basic sanity: the function pointers must be non-null.
    let _ = apis.create_app_container_profile as usize;
    let _ = apis.derive_app_container_sid_from_app_container_name as usize;
    let _ = apis.delete_app_container_profile as usize;
  }
}
