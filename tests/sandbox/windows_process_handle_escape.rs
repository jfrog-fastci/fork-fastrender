//! Windows sandbox security regression test: ensure the sandboxed child cannot obtain a process
//! handle to the parent (browser/broker) with high privileges.
//!
//! Threat model: even if filesystem/network are blocked, a compromised renderer could steal secrets
//! from the browser process by opening a handle with `PROCESS_VM_READ` / `PROCESS_DUP_HANDLE` and
//! reading memory or duplicating existing privileged handles.
//!
//! This test launches a child copy of the test harness inside the Windows renderer sandbox and then
//! attempts `OpenProcess(PROCESS_VM_READ | PROCESS_QUERY_INFORMATION | PROCESS_DUP_HANDLE)` against
//! the parent PID.
//!
//! We prefer running the child in an **AppContainer** (no capabilities). If AppContainer is
//! unavailable, we fall back to the project's **restricted-token + low integrity** mode. In either
//! case, `OpenProcess` must fail.

#![cfg(windows)]

use std::env;
use std::ffi::{c_void, OsStr};
use std::io;
use std::os::windows::ffi::OsStrExt;
use std::path::PathBuf;

use windows_sys::Win32::Foundation::{
  CloseHandle, GetLastError, SetHandleInformation, ERROR_ACCESS_DENIED, ERROR_INSUFFICIENT_BUFFER,
  HANDLE, HANDLE_FLAG_INHERIT, INVALID_HANDLE_VALUE,
};
use windows_sys::Win32::Security::Authorization::ConvertSidToStringSidW;
use windows_sys::Win32::Security::{
  ConvertStringSidToSidW, CreateRestrictedToken, FreeSid, GetLengthSid, GetTokenInformation,
  OpenProcessToken, SetTokenInformation, TokenCapabilities, TokenIntegrityLevel, TokenIsAppContainer,
  DISABLE_MAX_PRIVILEGE, SECURITY_CAPABILITIES, SE_GROUP_INTEGRITY, SE_GROUP_INTEGRITY_ENABLED,
  TOKEN_ADJUST_DEFAULT, TOKEN_ASSIGN_PRIMARY, TOKEN_DUPLICATE, TOKEN_GROUPS, TOKEN_INFORMATION_CLASS,
  TOKEN_MANDATORY_LABEL, TOKEN_QUERY,
};
use windows_sys::Win32::System::Console::{GetStdHandle, STD_ERROR_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE};
use windows_sys::Win32::System::Memory::LocalFree;
use windows_sys::Win32::System::Threading::{
  CreateProcessAsUserW, CreateProcessW, DeleteProcThreadAttributeList, GetCurrentProcess,
  GetExitCodeProcess, InitializeProcThreadAttributeList, OpenProcess, TerminateProcess,
  UpdateProcThreadAttribute, WaitForSingleObject, PROCESS_INFORMATION, PROCESS_DUP_HANDLE,
  PROCESS_QUERY_INFORMATION, PROCESS_VM_READ, PROC_THREAD_ATTRIBUTE_LIST, STARTUPINFOEXW,
};

const ENV_CHILD_MODE: &str = "FASTR_SANDBOX_TEST_CHILD";
const ENV_PARENT_PID: &str = "FASTR_SANDBOX_TEST_PARENT_PID";

/// The well-known capability SID for `internetClient`.
///
/// See: https://learn.microsoft.com/en-us/windows/security/identity-protection/access-control/security-identifiers#capability-sids
const INTERNET_CLIENT_CAPABILITY_SID: &str = "S-1-15-3-1";

// `PROC_THREAD_ATTRIBUTE_*` values are stable ABI constants from winbase.h, derived via:
//   ProcThreadAttributeValue(Number, Thread, Input, Additive)
// We define them here to avoid relying on a specific `windows-sys` version exporting them.
const PROC_THREAD_ATTRIBUTE_HANDLE_LIST: usize = 0x00020002;
const PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES: usize = 0x00020009;

const STARTF_USESTDHANDLES: u32 = 0x00000100;
const CREATE_UNICODE_ENVIRONMENT: u32 = 0x00000400;
const EXTENDED_STARTUPINFO_PRESENT: u32 = 0x00080000;

const WAIT_OBJECT_0: u32 = 0;
const WAIT_TIMEOUT: u32 = 0x00000102;
const WAIT_FAILED: u32 = 0xFFFF_FFFF;

const APP_CONTAINER_NAME: &str = "fastrender.sandbox.process-handle-escape-test";

// Some sandbox configurations may intentionally obscure process enumeration/opening by returning
// a different failure code (e.g. "invalid parameter" instead of "access denied"). Any of these are
// acceptable as long as `OpenProcess` fails.
const ERROR_INVALID_PARAMETER: u32 = 87;
const ERROR_PRIVILEGE_NOT_HELD: u32 = 1314;

#[derive(Debug)]
struct TokenState {
  is_app_container: bool,
  integrity_sid: String,
  integrity_rid: u32,
  capability_sids: Vec<String>,
}

impl TokenState {
  fn is_low_or_untrusted_integrity(&self) -> bool {
    matches!(self.integrity_rid, 0 | 4096)
  }

  fn has_internet_client_capability(&self) -> bool {
    self
      .capability_sids
      .iter()
      .any(|sid| sid.eq_ignore_ascii_case(INTERNET_CLIENT_CAPABILITY_SID))
  }
}

fn query_current_process_token_state() -> Result<TokenState, String> {
  let mut token: HANDLE = 0;
  // SAFETY: Win32 call; `token` is a valid output pointer.
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

  struct TokenGuard(HANDLE);
  impl Drop for TokenGuard {
    fn drop(&mut self) {
      unsafe {
        let _ = CloseHandle(self.0);
      }
    }
  }
  let token = TokenGuard(token);

  let is_app_container = query_token_is_app_container(token.0)?;
  let (integrity_sid, integrity_rid) = query_token_integrity_level(token.0)?;
  let capability_sids = if is_app_container {
    query_token_capabilities(token.0)?
  } else {
    Vec::new()
  };

  Ok(TokenState {
    is_app_container,
    integrity_sid,
    integrity_rid,
    capability_sids,
  })
}

fn query_token_is_app_container(token: HANDLE) -> Result<bool, String> {
  let mut value: u32 = 0;
  let mut returned: u32 = 0;
  // SAFETY: Win32 call; `value` and `returned` are valid output pointers.
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

fn query_token_integrity_level(token: HANDLE) -> Result<(String, u32), String> {
  let buf = get_token_information(token, TokenIntegrityLevel as TOKEN_INFORMATION_CLASS)?;
  if buf.len() < std::mem::size_of::<TOKEN_MANDATORY_LABEL>() {
    return Err(format!(
      "TokenIntegrityLevel buffer too small ({} bytes)",
      buf.len()
    ));
  }

  // SAFETY: buffer is large enough to contain TOKEN_MANDATORY_LABEL.
  let label = unsafe { &*(buf.as_ptr().cast::<TOKEN_MANDATORY_LABEL>()) };
  let sid = label.Label.Sid;
  if sid.is_null() {
    return Err("TokenIntegrityLevel returned null SID".to_string());
  }
  let sid_string = sid_to_string(sid)?;
  let rid = sid_string
    .rsplit('-')
    .next()
    .and_then(|tail| tail.parse::<u32>().ok())
    .ok_or_else(|| format!("unexpected integrity SID format: {sid_string}"))?;
  Ok((sid_string, rid))
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
    // SAFETY: buffer is sized for `count` entries.
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
  // SAFETY: size query; buffer may be null.
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
    // Some fixed-size token info classes succeed on the size query call.
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
  // SAFETY: `buf` is a valid output buffer.
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
  // SAFETY: Win32 call; writes `wide` on success (allocated with LocalAlloc).
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

// -----------------------------------------------------------------------------
// AppContainer dynamic loader
// -----------------------------------------------------------------------------
//
// AppContainer entrypoints are not present on older Windows releases. If we link them directly, the
// test binary may fail to start due to a missing import. Resolve them dynamically so the test can
// fall back to restricted-token sandboxing.

type HRESULT = i32;
type HMODULE = isize;

type CreateAppContainerProfileFn = unsafe extern "system" fn(
  app_container_name: *const u16,
  display_name: *const u16,
  description: *const u16,
  capabilities: *mut c_void,
  capability_count: u32,
  app_container_sid: *mut *mut c_void,
) -> HRESULT;

type DeriveAppContainerSidFromAppContainerNameFn =
  unsafe extern "system" fn(app_container_name: *const u16, app_container_sid: *mut *mut c_void)
    -> HRESULT;

#[link(name = "kernel32")]
extern "system" {
  fn LoadLibraryW(name: *const u16) -> HMODULE;
  fn GetProcAddress(module: HMODULE, proc_name: *const i8) -> *mut c_void;
  fn FreeLibrary(module: HMODULE) -> i32;
}

#[derive(Debug)]
struct UserenvModule(HMODULE);

impl Drop for UserenvModule {
  fn drop(&mut self) {
    unsafe {
      if self.0 != 0 {
        let _ = FreeLibrary(self.0);
      }
    }
  }
}

#[derive(Debug)]
struct UserenvApis {
  _module: UserenvModule,
  create_app_container_profile: CreateAppContainerProfileFn,
  derive_app_container_sid_from_app_container_name: DeriveAppContainerSidFromAppContainerNameFn,
}

unsafe fn get_userenv_proc<T>(module: HMODULE, symbol: &'static [u8], name: &'static str) -> Result<T, String> {
  let proc = GetProcAddress(module, symbol.as_ptr() as *const i8);
  if proc.is_null() {
    let err = GetLastError();
    return Err(format!(
      "userenv.dll missing required AppContainer symbol {name} (GetProcAddress err={err})"
    ));
  }
  Ok(std::mem::transmute_copy(&proc))
}

unsafe fn load_userenv_apis() -> Result<UserenvApis, String> {
  let dll_w = wide_null_str("userenv.dll");
  let module = LoadLibraryW(dll_w.as_ptr());
  if module == 0 {
    let err = GetLastError();
    return Err(format!("LoadLibraryW(userenv.dll) failed (err={err})"));
  }
  let module_guard = UserenvModule(module);

  let create_app_container_profile = get_userenv_proc::<CreateAppContainerProfileFn>(
    module,
    b"CreateAppContainerProfile\0",
    "CreateAppContainerProfile",
  )?;

  let derive_app_container_sid_from_app_container_name =
    get_userenv_proc::<DeriveAppContainerSidFromAppContainerNameFn>(
      module,
      b"DeriveAppContainerSidFromAppContainerName\0",
      "DeriveAppContainerSidFromAppContainerName",
    )?;

  Ok(UserenvApis {
    _module: module_guard,
    create_app_container_profile,
    derive_app_container_sid_from_app_container_name,
  })
}

fn wide_null(s: &OsStr) -> Vec<u16> {
  let mut wide: Vec<u16> = s.encode_wide().collect();
  wide.push(0);
  wide
}

fn wide_null_str(s: &str) -> Vec<u16> {
  let mut wide: Vec<u16> = s.encode_utf16().collect();
  wide.push(0);
  wide
}

fn build_environment_block(extra: &[(&str, String)]) -> Vec<u16> {
  let mut vars: Vec<(String, String)> = env::vars().collect();

  for (k, v) in extra {
    vars.retain(|(key, _)| key != k);
    vars.push(((*k).to_string(), v.clone()));
  }

  // Windows expects the environment block to be sorted by variable name.
  vars.sort_by(|a, b| a.0.to_ascii_uppercase().cmp(&b.0.to_ascii_uppercase()));

  let mut block: Vec<u16> = Vec::new();
  for (k, v) in vars {
    let entry = format!("{k}={v}");
    block.extend(entry.encode_utf16());
    block.push(0);
  }
  block.push(0); // Double-NUL terminator.
  block
}

fn system32_dir() -> String {
  // Use a working directory that is expected to be readable by an AppContainer process.
  let root = env::var("SystemRoot").unwrap_or_else(|_| "C:\\Windows".to_string());
  format!("{root}\\System32")
}

fn spawn_appcontainer_child(parent_pid: u32, test_filter: &str) -> Result<u32, String> {
  // We re-run the current test binary in "child mode", filtered down to this one test.
  let exe: PathBuf = env::current_exe().map_err(|e| format!("current_exe failed: {e}"))?;

  // libtest expects: <filter> --exact [--nocapture]
  let cmdline = format!(
    "\"{}\" {} --exact --nocapture",
    exe.display(),
    test_filter
  );

  let env_block = build_environment_block(&[
    (ENV_CHILD_MODE, "1".to_string()),
    (ENV_PARENT_PID, parent_pid.to_string()),
    // Helpful when debugging failures on CI.
    ("RUST_BACKTRACE", "1".to_string()),
  ]);

  let current_dir = system32_dir();

  unsafe {
    let userenv = load_userenv_apis()?;

    // Ensure the AppContainer profile exists (best-effort; older Windows builds may not support
    // AppContainer at all).
    let mut appcontainer_sid = std::ptr::null_mut();
    let appcontainer_name_w = wide_null_str(APP_CONTAINER_NAME);
    let display_name_w = wide_null_str("FastRender sandbox test");
    let description_w = wide_null_str("FastRender sandbox test AppContainer profile");

    let create_hr = (userenv.create_app_container_profile)(
      appcontainer_name_w.as_ptr(),
      display_name_w.as_ptr(),
      description_w.as_ptr(),
      std::ptr::null_mut(),
      0,
      &mut appcontainer_sid,
    );

    // `CreateAppContainerProfile` returns HRESULT_FROM_WIN32(ERROR_ALREADY_EXISTS) when the profile
    // already exists. In that case (or any other failure), derive the SID from the name.
    if create_hr != 0 {
      if (create_hr as u32) != 0x8007_00B7 {
        eprintln!(
          "CreateAppContainerProfile failed (hr=0x{create_hr:08X}); falling back to DeriveAppContainerSidFromAppContainerName"
        );
      }
      appcontainer_sid = std::ptr::null_mut();
      let derive_hr = (userenv.derive_app_container_sid_from_app_container_name)(
        appcontainer_name_w.as_ptr(),
        &mut appcontainer_sid,
      );
      if derive_hr != 0 {
        return Err(format!(
          "DeriveAppContainerSidFromAppContainerName failed (hr=0x{derive_hr:08X})"
        ));
      }
    }
    struct SidGuard(*mut std::ffi::c_void);
    impl Drop for SidGuard {
      fn drop(&mut self) {
        unsafe {
          if !self.0.is_null() {
            FreeSid(self.0);
          }
        }
      }
    }
    let _sid_guard = SidGuard(appcontainer_sid);

    let mut security_caps = SECURITY_CAPABILITIES {
      AppContainerSid: appcontainer_sid,
      Capabilities: std::ptr::null_mut(),
      CapabilityCount: 0,
      Reserved: 0,
    };

    // Limit handle inheritance to just the standard handles to avoid leaking any privileged
    // handles into the sandboxed process (which would undermine the test).
    let std_in = GetStdHandle(STD_INPUT_HANDLE);
    let std_out = GetStdHandle(STD_OUTPUT_HANDLE);
    let std_err = GetStdHandle(STD_ERROR_HANDLE);

    let mut handles: Vec<HANDLE> = Vec::new();
    for h in [std_in, std_out, std_err] {
      if h == 0 || h == INVALID_HANDLE_VALUE {
        continue;
      }
      // Ensure the handle is inheritable so it can be included in the handle list.
      let _ = SetHandleInformation(h, HANDLE_FLAG_INHERIT, HANDLE_FLAG_INHERIT);
      if !handles.contains(&h) {
        handles.push(h);
      }
    }

    let attribute_count: u32 = if handles.is_empty() { 1 } else { 2 };
    let mut attr_size: usize = 0;
    let _ = InitializeProcThreadAttributeList(std::ptr::null_mut(), attribute_count, 0, &mut attr_size);

    if attr_size == 0 {
      return Err("InitializeProcThreadAttributeList returned size=0".to_string());
    }

    let mut attr_buf = vec![0u8; attr_size];
    let attr_list = attr_buf.as_mut_ptr() as *mut PROC_THREAD_ATTRIBUTE_LIST;
    if InitializeProcThreadAttributeList(attr_list, attribute_count, 0, &mut attr_size) == 0 {
      let err = GetLastError();
      return Err(format!(
        "InitializeProcThreadAttributeList failed (err={err})"
      ));
    }
    struct AttrListGuard(*mut PROC_THREAD_ATTRIBUTE_LIST);
    impl Drop for AttrListGuard {
      fn drop(&mut self) {
        unsafe {
          if !self.0.is_null() {
            DeleteProcThreadAttributeList(self.0);
          }
        }
      }
    }
    let _attr_guard = AttrListGuard(attr_list);

    if UpdateProcThreadAttribute(
      attr_list,
      0,
      PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES,
      &mut security_caps as *mut _ as *mut std::ffi::c_void,
      std::mem::size_of::<SECURITY_CAPABILITIES>(),
      std::ptr::null_mut(),
      std::ptr::null_mut(),
    ) == 0
    {
      let err = GetLastError();
      return Err(format!(
        "UpdateProcThreadAttribute(PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES) failed (err={err})"
      ));
    }

    if !handles.is_empty() {
      if UpdateProcThreadAttribute(
        attr_list,
        0,
        PROC_THREAD_ATTRIBUTE_HANDLE_LIST,
        handles.as_mut_ptr() as *mut std::ffi::c_void,
        handles.len() * std::mem::size_of::<HANDLE>(),
        std::ptr::null_mut(),
        std::ptr::null_mut(),
      ) == 0
      {
        let err = GetLastError();
        return Err(format!(
          "UpdateProcThreadAttribute(PROC_THREAD_ATTRIBUTE_HANDLE_LIST) failed (err={err})"
        ));
      }
    }

    let mut si: STARTUPINFOEXW = std::mem::zeroed();
    si.StartupInfo.cb = std::mem::size_of::<STARTUPINFOEXW>() as u32;
    si.lpAttributeList = attr_list;

    if !handles.is_empty() {
      si.StartupInfo.dwFlags |= STARTF_USESTDHANDLES;
      si.StartupInfo.hStdInput = std_in;
      si.StartupInfo.hStdOutput = std_out;
      si.StartupInfo.hStdError = std_err;
    }

    let mut pi: PROCESS_INFORMATION = std::mem::zeroed();

    let exe_w = wide_null(exe.as_os_str());
    let mut cmd_w = wide_null_str(&cmdline);
    let current_dir_w = wide_null_str(&current_dir);

    let inherit_handles = if handles.is_empty() { 0 } else { 1 };
    let ok = CreateProcessW(
      exe_w.as_ptr(),
      cmd_w.as_mut_ptr(),
      std::ptr::null_mut(),
      std::ptr::null_mut(),
      inherit_handles,
      EXTENDED_STARTUPINFO_PRESENT | CREATE_UNICODE_ENVIRONMENT,
      env_block.as_ptr() as *mut std::ffi::c_void,
      current_dir_w.as_ptr(),
      &mut si.StartupInfo,
      &mut pi,
    );

    if ok == 0 {
      let err = GetLastError();
      return Err(format!(
        "CreateProcessW (AppContainer) failed (err={err}) for command line: {cmdline:?}"
      ));
    }

    struct HandleGuard(HANDLE);
    impl Drop for HandleGuard {
      fn drop(&mut self) {
        unsafe {
          if self.0 != 0 {
            let _ = CloseHandle(self.0);
          }
        }
      }
    }
    let _proc_guard = HandleGuard(pi.hProcess);
    let _thread_guard = HandleGuard(pi.hThread);

    // Wait up to 20 seconds for the sandboxed process to finish.
    let wait = WaitForSingleObject(pi.hProcess, 20_000);
    match wait {
      WAIT_OBJECT_0 => {}
      WAIT_TIMEOUT => {
        let _ = TerminateProcess(pi.hProcess, 1);
        return Err("sandboxed child timed out".to_string());
      }
      WAIT_FAILED => {
        let err = GetLastError();
        return Err(format!("WaitForSingleObject failed (err={err})"));
      }
      other => {
        return Err(format!("unexpected WaitForSingleObject result: {other}"));
      }
    }

    let mut exit_code: u32 = 0;
    if GetExitCodeProcess(pi.hProcess, &mut exit_code) == 0 {
      let err = GetLastError();
      return Err(format!("GetExitCodeProcess failed (err={err})"));
    }

    Ok(exit_code)
  }
}

fn set_low_integrity_token(token: HANDLE) -> Result<(), String> {
  unsafe {
    // Low integrity SID: S-1-16-4096.
    let sid_string = wide_null_str("S-1-16-4096");
    let mut sid: *mut std::ffi::c_void = std::ptr::null_mut();
    if ConvertStringSidToSidW(sid_string.as_ptr(), &mut sid) == 0 {
      let err = GetLastError();
      return Err(format!("ConvertStringSidToSidW failed (err={err})"));
    }
    if sid.is_null() {
      return Err("ConvertStringSidToSidW returned null SID".to_string());
    }
    struct SidGuard(*mut std::ffi::c_void);
    impl Drop for SidGuard {
      fn drop(&mut self) {
        unsafe {
          if !self.0.is_null() {
            // ConvertStringSidToSidW uses LocalAlloc.
            LocalFree(self.0 as isize);
          }
        }
      }
    }
    let _sid_guard = SidGuard(sid);

    let sid_len = GetLengthSid(sid) as usize;
    let tml_len = std::mem::size_of::<TOKEN_MANDATORY_LABEL>() + sid_len;

    // `TOKEN_MANDATORY_LABEL` contains pointers, so ensure pointer alignment (Vec<u8> is only 1-byte
    // aligned).
    let word_count = (tml_len + std::mem::size_of::<usize>() - 1) / std::mem::size_of::<usize>();
    let mut buffer_words = vec![0usize; word_count];
    let buffer_ptr = buffer_words.as_mut_ptr().cast::<u8>();

    let tml_ptr = buffer_ptr.cast::<TOKEN_MANDATORY_LABEL>();
    let sid_ptr = buffer_ptr.add(std::mem::size_of::<TOKEN_MANDATORY_LABEL>());
    (*tml_ptr).Label.Attributes = SE_GROUP_INTEGRITY | SE_GROUP_INTEGRITY_ENABLED;
    (*tml_ptr).Label.Sid = sid_ptr.cast();
    std::ptr::copy_nonoverlapping(sid.cast::<u8>(), sid_ptr, sid_len);

    let ok = SetTokenInformation(
      token,
      TokenIntegrityLevel as TOKEN_INFORMATION_CLASS,
      buffer_ptr.cast(),
      tml_len as u32,
    );
    if ok == 0 {
      let err = GetLastError();
      return Err(format!("SetTokenInformation(TokenIntegrityLevel) failed (err={err})"));
    }
    Ok(())
  }
}

fn spawn_restricted_token_child(parent_pid: u32, test_filter: &str) -> Result<u32, String> {
  let exe: PathBuf = env::current_exe().map_err(|e| format!("current_exe failed: {e}"))?;

  let cmdline = format!(
    "\"{}\" {} --exact --nocapture",
    exe.display(),
    test_filter
  );

  let env_block = build_environment_block(&[
    (ENV_CHILD_MODE, "1".to_string()),
    (ENV_PARENT_PID, parent_pid.to_string()),
    ("RUST_BACKTRACE", "1".to_string()),
  ]);

  let current_dir = system32_dir();

  unsafe {
    let mut token: HANDLE = 0;
    if OpenProcessToken(
      GetCurrentProcess(),
      TOKEN_DUPLICATE | TOKEN_QUERY | TOKEN_ASSIGN_PRIMARY | TOKEN_ADJUST_DEFAULT,
      &mut token,
    ) == 0
    {
      let err = GetLastError();
      return Err(format!("OpenProcessToken failed (err={err})"));
    }
    if token == 0 {
      return Err("OpenProcessToken returned null token handle".to_string());
    }
    struct HandleGuard(HANDLE);
    impl Drop for HandleGuard {
      fn drop(&mut self) {
        unsafe {
          if self.0 != 0 {
            let _ = CloseHandle(self.0);
          }
        }
      }
    }
    let _token_guard = HandleGuard(token);

    let mut restricted: HANDLE = 0;
    if CreateRestrictedToken(
      token,
      DISABLE_MAX_PRIVILEGE,
      0,
      std::ptr::null(),
      0,
      std::ptr::null(),
      0,
      std::ptr::null(),
      &mut restricted,
    ) == 0
    {
      let err = GetLastError();
      return Err(format!("CreateRestrictedToken failed (err={err})"));
    }
    if restricted == 0 {
      return Err("CreateRestrictedToken returned null token handle".to_string());
    }
    let _restricted_guard = HandleGuard(restricted);

    set_low_integrity_token(restricted)?;

    // Limit handle inheritance to just the standard handles.
    let std_in = GetStdHandle(STD_INPUT_HANDLE);
    let std_out = GetStdHandle(STD_OUTPUT_HANDLE);
    let std_err = GetStdHandle(STD_ERROR_HANDLE);

    let mut handles: Vec<HANDLE> = Vec::new();
    for h in [std_in, std_out, std_err] {
      if h == 0 || h == INVALID_HANDLE_VALUE {
        continue;
      }
      let _ = SetHandleInformation(h, HANDLE_FLAG_INHERIT, HANDLE_FLAG_INHERIT);
      if !handles.contains(&h) {
        handles.push(h);
      }
    }

    // Optional attribute list restricting inherited handles.
    struct AttrListGuard(*mut PROC_THREAD_ATTRIBUTE_LIST);
    impl Drop for AttrListGuard {
      fn drop(&mut self) {
        unsafe {
          if !self.0.is_null() {
            DeleteProcThreadAttributeList(self.0);
          }
        }
      }
    }

    let mut si_ex: STARTUPINFOEXW = std::mem::zeroed();
    si_ex.StartupInfo.cb = std::mem::size_of::<STARTUPINFOEXW>() as u32;
    let mut attr_buf: Vec<u8> = Vec::new();
    let mut attr_guard: Option<AttrListGuard> = None;

    if !handles.is_empty() {
      let attribute_count: u32 = 1;
      let mut attr_size: usize = 0;
      let _ = InitializeProcThreadAttributeList(
        std::ptr::null_mut(),
        attribute_count,
        0,
        &mut attr_size,
      );
      if attr_size == 0 {
        return Err("InitializeProcThreadAttributeList returned size=0".to_string());
      }

      attr_buf = vec![0u8; attr_size];
      let attr_list = attr_buf.as_mut_ptr() as *mut PROC_THREAD_ATTRIBUTE_LIST;
      if InitializeProcThreadAttributeList(attr_list, attribute_count, 0, &mut attr_size) == 0 {
        let err = GetLastError();
        return Err(format!("InitializeProcThreadAttributeList failed (err={err})"));
      }
      attr_guard = Some(AttrListGuard(attr_list));

      if UpdateProcThreadAttribute(
        attr_list,
        0,
        PROC_THREAD_ATTRIBUTE_HANDLE_LIST,
        handles.as_mut_ptr() as *mut std::ffi::c_void,
        handles.len() * std::mem::size_of::<HANDLE>(),
        std::ptr::null_mut(),
        std::ptr::null_mut(),
      ) == 0
      {
        let err = GetLastError();
        return Err(format!(
          "UpdateProcThreadAttribute(PROC_THREAD_ATTRIBUTE_HANDLE_LIST) failed (err={err})"
        ));
      }

      si_ex.lpAttributeList = attr_list;
      si_ex.StartupInfo.dwFlags |= STARTF_USESTDHANDLES;
      si_ex.StartupInfo.hStdInput = std_in;
      si_ex.StartupInfo.hStdOutput = std_out;
      si_ex.StartupInfo.hStdError = std_err;
    }

    let mut pi: PROCESS_INFORMATION = std::mem::zeroed();
    let exe_w = wide_null(exe.as_os_str());
    let mut cmd_w = wide_null_str(&cmdline);
    let current_dir_w = wide_null_str(&current_dir);

    let inherit_handles = if handles.is_empty() { 0 } else { 1 };
    let creation_flags =
      CREATE_UNICODE_ENVIRONMENT | if handles.is_empty() { 0 } else { EXTENDED_STARTUPINFO_PRESENT };

    let ok = CreateProcessAsUserW(
      restricted,
      exe_w.as_ptr(),
      cmd_w.as_mut_ptr(),
      std::ptr::null_mut(),
      std::ptr::null_mut(),
      inherit_handles,
      creation_flags,
      env_block.as_ptr() as *mut std::ffi::c_void,
      current_dir_w.as_ptr(),
      &mut si_ex.StartupInfo,
      &mut pi,
    );
    if ok == 0 {
      let err = GetLastError();
      return Err(format!(
        "CreateProcessAsUserW (restricted token) failed (err={err}) for command line: {cmdline:?}"
      ));
    }

    let _proc_guard = HandleGuard(pi.hProcess);
    let _thread_guard = HandleGuard(pi.hThread);

    let wait = WaitForSingleObject(pi.hProcess, 20_000);
    match wait {
      WAIT_OBJECT_0 => {}
      WAIT_TIMEOUT => {
        let _ = TerminateProcess(pi.hProcess, 1);
        return Err("sandboxed child timed out".to_string());
      }
      WAIT_FAILED => {
        let err = GetLastError();
        return Err(format!("WaitForSingleObject failed (err={err})"));
      }
      other => {
        return Err(format!("unexpected WaitForSingleObject result: {other}"));
      }
    }

    let mut exit_code: u32 = 0;
    if GetExitCodeProcess(pi.hProcess, &mut exit_code) == 0 {
      let err = GetLastError();
      return Err(format!("GetExitCodeProcess failed (err={err})"));
    }
    drop(attr_guard);
    Ok(exit_code)
  }
}

fn run_child() {
  let token = query_current_process_token_state()
    .unwrap_or_else(|err| panic!("failed to query sandbox token state in child: {err}"));
  eprintln!("sandbox: token_state={token:?}");
  if token.is_app_container {
    assert!(
      !token.has_internet_client_capability(),
      "SECURITY BUG: AppContainer token has internetClient capability ({INTERNET_CLIENT_CAPABILITY_SID}): {token:?}"
    );
  } else {
    assert!(
      token.is_low_or_untrusted_integrity(),
      "expected restricted-token fallback to run at Low/Untrusted integrity; token_state={token:?}"
    );
  }

  let parent_pid: u32 = env::var(ENV_PARENT_PID)
    .expect("child mode requires FASTR_SANDBOX_TEST_PARENT_PID")
    .parse()
    .expect("parent pid must parse as u32");

  let access: u32 = PROCESS_VM_READ | PROCESS_QUERY_INFORMATION | PROCESS_DUP_HANDLE;

  unsafe {
    let handle = OpenProcess(access, 0, parent_pid);
    if handle != 0 {
      let _ = CloseHandle(handle);
      panic!(
        "SECURITY BUG: OpenProcess succeeded against parent PID {parent_pid} with access mask 0x{access:08X}",
      );
    }

    let err = GetLastError();
    assert!(
      matches!(err, ERROR_ACCESS_DENIED | ERROR_INVALID_PARAMETER | ERROR_PRIVILEGE_NOT_HELD),
      "OpenProcess unexpectedly failed with error {err} (0x{err:08X}); expected ERROR_ACCESS_DENIED (5) or equivalent. Requested access mask: 0x{access:08X}",
    );
  }
}

#[test]
fn sandboxed_renderer_cannot_open_parent_process_handle() {
  // Child mode: run the attack attempt inside the sandbox.
  if env::var_os(ENV_CHILD_MODE).is_some() {
    run_child();
    return;
  }

  // Parent mode: spawn the sandboxed child and assert it cannot open us.
  let parent_pid = std::process::id();
  let test_filter =
    "sandbox::windows_process_handle_escape::sandboxed_renderer_cannot_open_parent_process_handle";

  let exit = match spawn_appcontainer_child(parent_pid, test_filter) {
    Ok(exit) => exit,
    Err(appcontainer_err) => {
      eprintln!(
        "AppContainer spawn failed ({appcontainer_err}); falling back to restricted-token sandbox"
      );
      spawn_restricted_token_child(parent_pid, test_filter)
        .expect("spawn sandboxed child (restricted token)")
    }
  };
  if exit != 0 {
    panic!("sandboxed child exited with code {exit}");
  }
}
