#![cfg(windows)]

use std::ffi::OsString;
use std::io;
use std::os::windows::io::{AsRawHandle, RawHandle};

use fastrender::sandbox::windows::{spawn_sandboxed_with_config, SpawnConfig, WindowsSandboxLevel};
use windows_sys::Win32::Foundation::{
  CloseHandle, GetHandleInformation, GetLastError, SetHandleInformation, ERROR_INSUFFICIENT_BUFFER,
  HANDLE, HANDLE_FLAG_INHERIT, INVALID_HANDLE_VALUE,
};
use windows_sys::Win32::Security::Authorization::ConvertSidToStringSidW;
use windows_sys::Win32::Security::{
  GetTokenInformation, OpenProcessToken, TokenGroups, TokenIsAppContainer, TOKEN_GROUPS,
  TOKEN_INFORMATION_CLASS, TOKEN_QUERY,
};
use windows_sys::Win32::System::Memory::LocalFree;
use windows_sys::Win32::System::Threading::{
  DeleteProcThreadAttributeList, GetCurrentProcess, InitializeProcThreadAttributeList,
  UpdateProcThreadAttribute, PROC_THREAD_ATTRIBUTE_LIST,
};

// Well-known group SID granted broad access to some system objects for packaged apps.
const ALL_APPLICATION_PACKAGES_SID: &str = "S-1-15-2-1";

// STARTUPINFOEX attribute value:
// ProcThreadAttributeValue(15, FALSE, TRUE, FALSE) → 0x0002_000F.
const PROC_THREAD_ATTRIBUTE_ALL_APPLICATION_PACKAGES_POLICY: usize = 0x0002_000F;

// Value for `PROC_THREAD_ATTRIBUTE_ALL_APPLICATION_PACKAGES_POLICY` (winbase.h).
const PROCESS_CREATION_ALL_APPLICATION_PACKAGES_POLICY_BLOCK: u32 = 1;

#[test]
fn appcontainer_token_omits_all_application_packages_group_when_hardened() {
  if !crate::common::windows_sandbox::require_full_windows_sandbox(
    "appcontainer_token_omits_all_application_packages_group_when_hardened",
  ) {
    return;
  }

  // Ensure developer environment overrides don't silently change test semantics.
  let _env_guard = crate::common::EnvVarsGuard::remove(&[
    "FASTR_DISABLE_RENDERER_SANDBOX",
    "FASTR_WINDOWS_RENDERER_SANDBOX",
    "FASTR_ALLOW_UNSANDBOXED_RENDERER",
  ]);

  let exe = std::env::current_exe().expect("current test exe path");
  let child_test_name = concat!(
    module_path!(),
    "::appcontainer_all_application_packages_hardening_child"
  );

  let args = vec![
    OsString::from("--ignored"),
    OsString::from("--exact"),
    OsString::from(child_test_name),
    OsString::from("--nocapture"),
  ];

  let stdout = std::io::stdout().as_raw_handle();
  let stderr = std::io::stderr().as_raw_handle();

  let mut handles: Vec<RawHandle> = Vec::new();
  let mut to_make_inheritable: Vec<HANDLE> = Vec::new();
  if !stdout.is_null() {
    handles.push(stdout);
    to_make_inheritable.push(stdout as HANDLE);
  }
  if !stderr.is_null() && stderr != stdout {
    handles.push(stderr);
    to_make_inheritable.push(stderr as HANDLE);
  }
  let _inherit_guard = HandleInheritGuard::new(&to_make_inheritable);

  let child = spawn_sandboxed_with_config(
    &exe,
    &args,
    &handles,
    SpawnConfig {
      all_application_packages_hardened: true,
    },
  )
  .expect("spawn sandboxed child");

  let level = child.level;
  assert_eq!(
    level,
    WindowsSandboxLevel::AppContainer,
    "expected AppContainer sandboxing (no silent fallback)"
  );
  let exit_code = child.wait().expect("wait for sandboxed child");
  assert_eq!(
    exit_code, 0,
    "sandboxed child exited with code {exit_code} (sandbox_level={level:?})"
  );
}

#[test]
#[ignore]
fn appcontainer_all_application_packages_hardening_child() {
  let mut token: HANDLE = 0;
  let ok = unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) };
  assert_ne!(
    ok,
    0,
    "OpenProcessToken failed: {}",
    io::Error::last_os_error()
  );
  assert_ne!(token, 0, "OpenProcessToken returned null token handle");

  struct TokenHandle(HANDLE);
  impl Drop for TokenHandle {
    fn drop(&mut self) {
      unsafe {
        let _ = CloseHandle(self.0);
      }
    }
  }
  let token = TokenHandle(token);

  let is_appcontainer = query_token_is_app_container(token.0).expect("query TokenIsAppContainer");
  assert!(
    is_appcontainer,
    "expected child token to be an AppContainer token (no silent fallback)"
  );

  let groups = query_token_groups(token.0).expect("query TokenGroups");
  let has_aap = groups
    .iter()
    .any(|sid| sid.eq_ignore_ascii_case(ALL_APPLICATION_PACKAGES_SID));

  if has_aap {
    let supported = is_all_application_packages_policy_supported()
      .expect("check PROC_THREAD_ATTRIBUTE_ALL_APPLICATION_PACKAGES_POLICY support");
    if !supported {
      eprintln!(
        "skipping: host Windows build rejected PROC_THREAD_ATTRIBUTE_ALL_APPLICATION_PACKAGES_POLICY; \
AppContainer token still contains {ALL_APPLICATION_PACKAGES_SID} group"
      );
      return;
    }

    panic!(
      "expected AppContainer token to NOT contain ALL APPLICATION PACKAGES ({ALL_APPLICATION_PACKAGES_SID}) \
when hardening is enabled; token groups={groups:?}"
    );
  }
}

struct HandleInheritGuard {
  saved: Vec<(HANDLE, u32)>,
}

impl HandleInheritGuard {
  fn new(handles: &[HANDLE]) -> Self {
    let mut saved = Vec::with_capacity(handles.len());
    for handle in handles {
      if *handle == 0 || *handle == INVALID_HANDLE_VALUE {
        continue;
      }
      let mut flags: u32 = 0;
      let ok = unsafe { GetHandleInformation(*handle, &mut flags) };
      if ok == 0 {
        continue;
      }
      saved.push((*handle, flags));
      let _ = unsafe { SetHandleInformation(*handle, HANDLE_FLAG_INHERIT, HANDLE_FLAG_INHERIT) };
    }
    Self { saved }
  }
}

impl Drop for HandleInheritGuard {
  fn drop(&mut self) {
    for (handle, flags) in self.saved.drain(..) {
      let inherit = if (flags & HANDLE_FLAG_INHERIT) != 0 {
        HANDLE_FLAG_INHERIT
      } else {
        0
      };
      let _ = unsafe { SetHandleInformation(handle, HANDLE_FLAG_INHERIT, inherit) };
    }
  }
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

fn query_token_groups(token: HANDLE) -> Result<Vec<String>, String> {
  let buf = get_token_information(token, TokenGroups as TOKEN_INFORMATION_CLASS)?;
  if buf.is_empty() {
    return Ok(Vec::new());
  }
  if buf.len() < std::mem::size_of::<TOKEN_GROUPS>() {
    return Err(format!(
      "TokenGroups buffer too small ({} bytes)",
      buf.len()
    ));
  }

  // SAFETY: buffer is large enough for TOKEN_GROUPS header.
  let groups = unsafe { &*(buf.as_ptr().cast::<TOKEN_GROUPS>()) };
  let count = groups.GroupCount as usize;

  // TOKEN_GROUPS is variable length. windows-sys models it with a 1-element array; walk with
  // pointer arithmetic.
  let first = groups.Groups.as_ptr();
  let mut out = Vec::with_capacity(count);
  for idx in 0..count {
    // SAFETY: buffer returned by GetTokenInformation is sized for `count` entries.
    let entry = unsafe { &*first.add(idx) };
    if entry.Sid.is_null() {
      continue;
    }
    out.push(sid_to_string(entry.Sid)?);
  }
  Ok(out)
}

fn get_token_information(token: HANDLE, class: TOKEN_INFORMATION_CLASS) -> Result<Vec<u8>, String> {
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
    // Unexpected but possible for fixed-size info classes.
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
      buf.as_mut_ptr().cast(),
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

fn sid_to_string(sid: *mut std::ffi::c_void) -> Result<String, String> {
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

fn is_all_application_packages_policy_supported() -> Result<bool, String> {
  // Query required size.
  let mut size: usize = 0;
  unsafe {
    InitializeProcThreadAttributeList(std::ptr::null_mut(), 1, 0, &mut size);
  }
  if size == 0 {
    return Ok(false);
  }

  // Allocate a suitably aligned backing buffer.
  let word_count = (size + std::mem::size_of::<u64>() - 1) / std::mem::size_of::<u64>();
  let mut buffer = vec![0u64; word_count];
  let list = buffer.as_mut_ptr().cast::<PROC_THREAD_ATTRIBUTE_LIST>();

  let ok = unsafe { InitializeProcThreadAttributeList(list, 1, 0, &mut size) };
  if ok == 0 {
    return Err(format!(
      "InitializeProcThreadAttributeList failed: {}",
      io::Error::last_os_error()
    ));
  }

  let mut policy_value = PROCESS_CREATION_ALL_APPLICATION_PACKAGES_POLICY_BLOCK;
  let ok = unsafe {
    UpdateProcThreadAttribute(
      list,
      0,
      PROC_THREAD_ATTRIBUTE_ALL_APPLICATION_PACKAGES_POLICY,
      std::ptr::addr_of_mut!(policy_value).cast(),
      std::mem::size_of::<u32>(),
      std::ptr::null_mut(),
      std::ptr::null_mut(),
    )
  };

  if ok != 0 {
    unsafe {
      DeleteProcThreadAttributeList(list);
    }
    return Ok(true);
  }

  let code = unsafe { GetLastError() };
  unsafe {
    DeleteProcThreadAttributeList(list);
  }

  // ERROR_NOT_SUPPORTED (50) / ERROR_INVALID_PARAMETER (87) => attribute not available.
  if code == 50 || code == 87 {
    return Ok(false);
  }

  Err(format!(
    "UpdateProcThreadAttribute(PROC_THREAD_ATTRIBUTE_ALL_APPLICATION_PACKAGES_POLICY) failed with Win32 error {code}"
  ))
}
