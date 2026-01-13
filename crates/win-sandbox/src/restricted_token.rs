use crate::{OwnedHandle, OwnedSid, Result, WinSandboxError};

use std::ffi::OsStr;
use std::os::windows::ffi::OsStrExt;

use windows_sys::Win32::Foundation::HANDLE;
use windows_sys::Win32::Security::Authorization::ConvertStringSidToSidW;
use windows_sys::Win32::Security::{
  CreateRestrictedToken, GetLengthSid, SetTokenInformation, TokenIntegrityLevel,
  DISABLE_MAX_PRIVILEGE, TOKEN_ADJUST_DEFAULT, TOKEN_ASSIGN_PRIMARY, TOKEN_DUPLICATE,
  TOKEN_INFORMATION_CLASS, TOKEN_MANDATORY_LABEL, TOKEN_QUERY,
};
use windows_sys::Win32::System::SystemServices::{SE_GROUP_INTEGRITY, SE_GROUP_INTEGRITY_ENABLED};
use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

/// A restricted primary token suitable for spawning a sandboxed child process.
///
/// This is the "fallback" Windows sandbox mode used when AppContainer is unavailable.
///
/// The token is created via `CreateRestrictedToken(DISABLE_MAX_PRIVILEGE)` and then configured with
/// a **low integrity** mandatory label. The resulting token should be used with
/// `CreateProcessAsUserW`.
#[derive(Debug)]
pub struct RestrictedToken {
  token: OwnedHandle,
}

impl RestrictedToken {
  /// Creates a restricted token derived from the current process token.
  ///
  /// The returned token is suitable for use as a primary token in `CreateProcessAsUserW`.
  pub fn for_current_process_low_integrity() -> Result<Self> {
    let token = open_current_process_token()?;
    let restricted = create_restricted_token_disable_max_privilege(token.as_raw())?;
    set_low_integrity(restricted.as_raw())?;
    Ok(Self { token: restricted })
  }

  /// Returns the underlying Win32 `HANDLE` for the token.
  pub fn handle(&self) -> HANDLE {
    self.token.as_raw()
  }

  /// Consumes the wrapper and returns the underlying owned handle.
  pub fn into_handle(self) -> OwnedHandle {
    self.token
  }
}

fn open_current_process_token() -> Result<OwnedHandle> {
  let mut token: HANDLE = std::ptr::null_mut();

  // SAFETY: FFI call. Output handle pointer is valid.
  let ok = unsafe {
    OpenProcessToken(
      GetCurrentProcess(),
      TOKEN_DUPLICATE | TOKEN_QUERY | TOKEN_ASSIGN_PRIMARY | TOKEN_ADJUST_DEFAULT,
      &mut token,
    )
  };
  if ok == 0 {
    return Err(WinSandboxError::last("OpenProcessToken"));
  }
  if token.is_null() {
    return Err(WinSandboxError::NullPointer {
      func: "OpenProcessToken",
    });
  }

  Ok(OwnedHandle::from_raw(token))
}

fn create_restricted_token_disable_max_privilege(existing_token: HANDLE) -> Result<OwnedHandle> {
  let mut restricted: HANDLE = std::ptr::null_mut();

  // SAFETY: FFI call.
  let ok = unsafe {
    CreateRestrictedToken(
      existing_token,
      DISABLE_MAX_PRIVILEGE,
      0,
      std::ptr::null(),
      0,
      std::ptr::null(),
      0,
      std::ptr::null(),
      &mut restricted,
    )
  };
  if ok == 0 {
    return Err(WinSandboxError::last("CreateRestrictedToken"));
  }
  if restricted.is_null() {
    return Err(WinSandboxError::NullPointer {
      func: "CreateRestrictedToken",
    });
  }
  Ok(OwnedHandle::from_raw(restricted))
}

fn set_low_integrity(token: HANDLE) -> Result<()> {
  // Low integrity SID: S-1-16-4096.
  let sid_string = wide_null(OsStr::new("S-1-16-4096"));
  let mut sid: windows_sys::Win32::Security::PSID = std::ptr::null_mut();

  // SAFETY: FFI call.
  let ok = unsafe { ConvertStringSidToSidW(sid_string.as_ptr(), &mut sid) };
  if ok == 0 {
    return Err(WinSandboxError::last("ConvertStringSidToSidW"));
  }
  if sid.is_null() {
    return Err(WinSandboxError::NullPointer {
      func: "ConvertStringSidToSidW",
    });
  }

  // `ConvertStringSidToSidW` returns memory that must be freed with `LocalFree`.
  let sid = OwnedSid::from_local_free(sid);

  // SAFETY: The SID pointer is valid.
  let sid_len = unsafe { GetLengthSid(sid.as_ptr()) } as usize;
  let tml_len = std::mem::size_of::<TOKEN_MANDATORY_LABEL>() + sid_len;

  // `TOKEN_MANDATORY_LABEL` contains pointers, so ensure the backing buffer has at least pointer
  // alignment. `Vec<u8>` only guarantees 1-byte alignment.
  let word_count = (tml_len + std::mem::size_of::<usize>() - 1) / std::mem::size_of::<usize>();
  let mut buffer_words = vec![0usize; word_count];
  let buffer_ptr = buffer_words.as_mut_ptr().cast::<u8>();

  let tml_ptr = buffer_ptr.cast::<TOKEN_MANDATORY_LABEL>();
  let sid_ptr = unsafe { buffer_ptr.add(std::mem::size_of::<TOKEN_MANDATORY_LABEL>()) };

  unsafe {
    (*tml_ptr).Label.Attributes = (SE_GROUP_INTEGRITY | SE_GROUP_INTEGRITY_ENABLED) as u32;
    (*tml_ptr).Label.Sid = sid_ptr.cast();
    std::ptr::copy_nonoverlapping(sid.as_ptr().cast::<u8>(), sid_ptr, sid_len);
  }

  // SAFETY: `buffer_ptr` points to a valid `TOKEN_MANDATORY_LABEL` followed by a SID.
  let ok = unsafe {
    SetTokenInformation(
      token,
      TokenIntegrityLevel as TOKEN_INFORMATION_CLASS,
      buffer_ptr.cast(),
      tml_len as u32,
    )
  };
  if ok == 0 {
    return Err(WinSandboxError::last("SetTokenInformation(TokenIntegrityLevel)"));
  }

  Ok(())
}

fn wide_null(value: &OsStr) -> Vec<u16> {
  value.encode_wide().chain(Some(0)).collect()
}
