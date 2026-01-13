//! Helper binary for Windows AppContainer sandbox integration tests.
//!
//! This binary is intentionally tiny and dependency-free so it can be spawned inside an
//! AppContainer and validate that:
//! - the process has an accessible current working directory, and
//! - `std::env::temp_dir()` points to a writable location.

use std::fs;
use std::io::{Read, Write};
use std::path::PathBuf;

fn main() {
  if let Err(err) = run() {
    eprintln!("{err}");
    std::process::exit(1);
  }
}

fn run() -> Result<(), String> {
  // Ensure the process is actually running under the expected AppContainer token. Without this
  // check, regressions in the sandbox spawn path could cause this probe to pass while running
  // unsandboxed.
  #[cfg(windows)]
  {
    windows_token::assert_current_process_is_appcontainer_without_internet_client()?;
  }

  // Validate that the current directory is at least accessible (some libs do relative path I/O,
  // probe the cwd, etc). We don't require that it is writable (e.g. fallback CWD may be System32).
  fs::read_dir(".")
    .map_err(|err| format!("failed to read current directory inside sandbox: {err}"))?
    .next();

  let temp_dir = std::env::temp_dir();
  if temp_dir.as_os_str().is_empty() {
    return Err("std::env::temp_dir() returned empty path".to_string());
  }

  // Create a unique temp file and validate basic read/write/delete operations.
  let file_path = unique_temp_file_path(&temp_dir);
  let payload = b"fastrender-appcontainer-temp-smoke";

  {
    let mut file = fs::File::create(&file_path)
      .map_err(|err| format!("failed to create temp file {}: {err}", file_path.display()))?;
    file
      .write_all(payload)
      .map_err(|err| format!("failed to write temp file {}: {err}", file_path.display()))?;
    file
      .flush()
      .map_err(|err| format!("failed to flush temp file {}: {err}", file_path.display()))?;
  }

  let mut read_back = Vec::new();
  {
    let mut file = fs::File::open(&file_path)
      .map_err(|err| format!("failed to open temp file {}: {err}", file_path.display()))?;
    file
      .read_to_end(&mut read_back)
      .map_err(|err| format!("failed to read temp file {}: {err}", file_path.display()))?;
  }
  if read_back != payload {
    return Err(format!(
      "temp file {} round-trip mismatch (expected {:?}, got {:?})",
      file_path.display(),
      payload,
      read_back
    ));
  }

  fs::remove_file(&file_path)
    .map_err(|err| format!("failed to delete temp file {}: {err}", file_path.display()))?;

  Ok(())
}

fn unique_temp_file_path(temp_dir: &PathBuf) -> PathBuf {
  let nanos = std::time::SystemTime::now()
    .duration_since(std::time::UNIX_EPOCH)
    .unwrap_or_default()
    .as_nanos();
  temp_dir.join(format!(
    "fastrender_appcontainer_temp_smoke_{}_{}.tmp",
    std::process::id(),
    nanos
  ))
}

#[cfg(windows)]
mod windows_token {
  use std::ffi::c_void;
  use std::io;

  use windows_sys::Win32::Foundation::{CloseHandle, ERROR_INSUFFICIENT_BUFFER, HANDLE};
  use windows_sys::Win32::Security::Authorization::ConvertSidToStringSidW;
  use windows_sys::Win32::Security::{
    GetTokenInformation, OpenProcessToken, TokenCapabilities, TokenIsAppContainer, TOKEN_GROUPS,
    TOKEN_INFORMATION_CLASS, TOKEN_QUERY,
  };
  use windows_sys::Win32::System::Memory::LocalFree;
  use windows_sys::Win32::System::Threading::GetCurrentProcess;

  /// The well-known capability SID for `internetClient`.
  ///
  /// See: https://learn.microsoft.com/en-us/windows/security/identity-protection/access-control/security-identifiers#capability-sids
  const INTERNET_CLIENT_CAPABILITY_SID: &str = "S-1-15-3-1";

  pub(super) fn assert_current_process_is_appcontainer_without_internet_client(
  ) -> Result<(), String> {
    let token = open_current_process_token()?;
    let _guard = TokenHandle(token);

    let is_app_container = query_token_is_app_container(token)?;
    if !is_app_container {
      return Err(
        "expected sandboxed child to run with an AppContainer token (TokenIsAppContainer=1)"
          .to_string(),
      );
    }

    let caps = query_token_capabilities(token)?;
    if caps
      .iter()
      .any(|sid| sid.eq_ignore_ascii_case(INTERNET_CLIENT_CAPABILITY_SID))
    {
      return Err(format!(
        "SECURITY BUG: AppContainer token has internetClient capability ({INTERNET_CLIENT_CAPABILITY_SID}): capabilities={caps:?}"
      ));
    }

    Ok(())
  }

  struct TokenHandle(HANDLE);

  impl Drop for TokenHandle {
    fn drop(&mut self) {
      unsafe {
        let _ = CloseHandle(self.0);
      }
    }
  }

  fn open_current_process_token() -> Result<HANDLE, String> {
    let mut token: HANDLE = 0;
    let ok = unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) };
    if ok == 0 {
      return Err(format!(
        "OpenProcessToken(GetCurrentProcess, TOKEN_QUERY) failed: {}",
        io::Error::last_os_error()
      ));
    }
    if token == 0 {
      return Err("OpenProcessToken returned null token handle".to_string());
    }
    Ok(token)
  }

  fn query_token_is_app_container(token: HANDLE) -> Result<bool, String> {
    let mut value: u32 = 0;
    let mut returned: u32 = 0;
    let ok = unsafe {
      GetTokenInformation(
        token,
        TokenIsAppContainer as TOKEN_INFORMATION_CLASS,
        std::ptr::addr_of_mut!(value).cast(),
        std::mem::size_of::<u32>() as u32,
        std::ptr::addr_of_mut!(returned),
      )
    };
    if ok == 0 {
      return Err(format!(
        "GetTokenInformation(TokenIsAppContainer) failed: {}",
        io::Error::last_os_error()
      ));
    }
    Ok(value != 0)
  }

  fn query_token_capabilities(token: HANDLE) -> Result<Vec<String>, String> {
    let buf = get_token_information(token, TokenCapabilities as TOKEN_INFORMATION_CLASS)?;
    if buf.is_empty() {
      return Ok(Vec::new());
    }
    if buf.len() < std::mem::size_of::<TOKEN_GROUPS>() {
      return Err(format!(
        "TokenCapabilities buffer too small ({} bytes)",
        buf.len()
      ));
    }

    // SAFETY: buffer is large enough for TOKEN_GROUPS header.
    let groups = unsafe { &*(buf.as_ptr().cast::<TOKEN_GROUPS>()) };
    let count = groups.GroupCount as usize;
    let first = groups.Groups.as_ptr();
    let mut out = Vec::new();
    for idx in 0..count {
      // SAFETY: buffer returned by GetTokenInformation is sized for `count` entries.
      let entry = unsafe { &*first.add(idx) };
      if entry.Sid.is_null() {
        continue;
      }
      out.push(sid_to_string(entry.Sid.cast())?);
    }
    Ok(out)
  }

  fn get_token_information(
    token: HANDLE,
    class: TOKEN_INFORMATION_CLASS,
  ) -> Result<Vec<u8>, String> {
    let mut needed: u32 = 0;
    let ok = unsafe {
      GetTokenInformation(
        token,
        class,
        std::ptr::null_mut(),
        0,
        std::ptr::addr_of_mut!(needed),
      )
    };
    if ok != 0 {
      // Fixed-size class.
      return Ok(Vec::new());
    }

    let err = io::Error::last_os_error();
    if err.raw_os_error() != Some(ERROR_INSUFFICIENT_BUFFER as i32) {
      return Err(format!(
        "GetTokenInformation(size query) failed: {err} (raw_os_error={:?})",
        err.raw_os_error()
      ));
    }
    if needed == 0 {
      return Err(
        "GetTokenInformation returned ERROR_INSUFFICIENT_BUFFER but length was 0".to_string(),
      );
    }

    let mut buf = vec![0u8; needed as usize];
    let ok = unsafe {
      GetTokenInformation(
        token,
        class,
        buf.as_mut_ptr().cast::<c_void>(),
        needed,
        std::ptr::addr_of_mut!(needed),
      )
    };
    if ok == 0 {
      return Err(format!(
        "GetTokenInformation(data) failed: {}",
        io::Error::last_os_error()
      ));
    }
    buf.truncate(needed as usize);
    Ok(buf)
  }

  fn sid_to_string(sid: *mut c_void) -> Result<String, String> {
    let mut wide: *mut u16 = std::ptr::null_mut();
    let ok = unsafe { ConvertSidToStringSidW(sid, std::ptr::addr_of_mut!(wide)) };
    if ok == 0 {
      return Err(format!(
        "ConvertSidToStringSidW failed: {}",
        io::Error::last_os_error()
      ));
    }
    if wide.is_null() {
      return Err("ConvertSidToStringSidW succeeded but returned null pointer".to_string());
    }

    // SAFETY: pointer is NUL-terminated per Win32 contract.
    let mut len = 0usize;
    unsafe {
      while *wide.add(len) != 0 {
        len += 1;
      }
      let slice = std::slice::from_raw_parts(wide, len);
      let s = String::from_utf16_lossy(slice);
      LocalFree(wide as isize);
      Ok(s)
    }
  }
}
