//! Windows sandbox regression test: AppContainer processes should not have network access.
//!
//! The preferred Windows renderer sandbox is an AppContainer with **zero capabilities** (no
//! `INTERNET_CLIENT`), which should deny outbound networking. This test validates that a sandboxed
//! child cannot establish a TCP connection to a localhost listener owned by the parent.
//!
//! If AppContainer sandboxing is unavailable on the current host (older Windows versions / stripped
//! Server configs), this test prints a clear skip message rather than failing the suite.

#![cfg(windows)]

use std::ffi::OsString;
use std::io;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::os::windows::io::{AsRawHandle, RawHandle};
use std::time::Duration;

use fastrender::sandbox::windows::spawn_sandboxed;
use windows_sys::Win32::Foundation::{
  CloseHandle, GetHandleInformation, SetHandleInformation, ERROR_INSUFFICIENT_BUFFER, HANDLE,
  HANDLE_FLAG_INHERIT, INVALID_HANDLE_VALUE,
};
use windows_sys::Win32::Security::Authorization::ConvertSidToStringSidW;
use windows_sys::Win32::Security::{
  GetTokenInformation, OpenProcessToken, TokenCapabilities, TokenIntegrityLevel,
  TokenIsAppContainer, TOKEN_GROUPS, TOKEN_INFORMATION_CLASS, TOKEN_MANDATORY_LABEL, TOKEN_QUERY,
};
use windows_sys::Win32::System::Console::{
  GetStdHandle, STD_ERROR_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE,
};
use windows_sys::Win32::System::Memory::LocalFree;
use windows_sys::Win32::System::Threading::{
  GetCurrentProcess, GetExitCodeProcess, TerminateProcess, WaitForSingleObject,
};

const ENV_PORT: &str = "FASTR_TEST_WIN_SANDBOX_NETWORK_PORT";
const WSAEACCES: i32 = 10013;

/// The well-known capability SID for `internetClient`.
///
/// See: https://learn.microsoft.com/en-us/windows/security/identity-protection/access-control/security-identifiers#capability-sids
const INTERNET_CLIENT_CAPABILITY_SID: &str = "S-1-15-3-1";

const WAIT_OBJECT_0: u32 = 0x0000_0000;
const WAIT_TIMEOUT: u32 = 0x0000_0102;

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
  let ok = unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) };
  if ok == 0 || token == 0 || token == INVALID_HANDLE_VALUE {
    return Err(format!(
      "OpenProcessToken(GetCurrentProcess, TOKEN_QUERY) failed: {}",
      io::Error::last_os_error()
    ));
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
    // Unexpected but possible for fixed-size token info classes.
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
    LocalFree(wide as _);
    Ok(s)
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

fn collect_stdio_handles_for_inheritance() -> (Vec<RawHandle>, HandleInheritGuard) {
  let std_in = unsafe { GetStdHandle(STD_INPUT_HANDLE) };
  let std_out = unsafe { GetStdHandle(STD_OUTPUT_HANDLE) };
  let std_err = unsafe { GetStdHandle(STD_ERROR_HANDLE) };

  let mut handles: Vec<HANDLE> = Vec::new();
  for h in [std_in, std_out, std_err] {
    if h == 0 || h == INVALID_HANDLE_VALUE {
      continue;
    }
    if !handles.contains(&h) {
      handles.push(h);
    }
  }

  let guard = HandleInheritGuard::new(&handles);
  let inherit = handles.iter().copied().map(|h| h as RawHandle).collect();
  (inherit, guard)
}

#[test]
fn appcontainer_denies_outbound_tcp_connect() {
  if !crate::common::windows_sandbox::require_full_windows_sandbox(
    "appcontainer_denies_outbound_tcp_connect",
  ) {
    return;
  }

  // Serialize network-heavy tests to keep CI deterministic.
  let _net_guard = crate::common::net_test_lock();

  // Binding localhost can fail in some CI environments; skip with a clear message.
  let listener: TcpListener =
    match crate::common::try_bind_localhost("appcontainer_denies_outbound_tcp_connect") {
      Some(listener) => listener,
      None => return,
    };
  let port = listener.local_addr().map(|addr| addr.port()).unwrap_or(0);
  assert!(port != 0, "expected listener to have a non-zero port");

  let exe = std::env::current_exe().expect("current test exe path");
  let test_name = "sandbox::windows_network_denial::appcontainer_network_denied_child";
  let args = vec![
    OsString::from("--ignored"),
    OsString::from("--exact"),
    OsString::from(test_name),
    OsString::from("--nocapture"),
  ];

  let port_str = port.to_string();
  let (inherit, _inherit_guard) = collect_stdio_handles_for_inheritance();
  let child = {
    let _env_guard = crate::common::EnvVarsGuard::new(&[
      ("FASTR_DISABLE_RENDERER_SANDBOX", None),
      ("FASTR_WINDOWS_RENDERER_SANDBOX", None),
      ("FASTR_ALLOW_UNSANDBOXED_RENDERER", None),
      ("FASTR_DISABLE_WIN_MITIGATIONS", None),
      (ENV_PORT, Some(port_str.as_str())),
    ]);
    spawn_sandboxed(&exe, &args, &inherit).expect("spawn sandboxed child")
  };

  let handle = child.process.as_raw_handle() as HANDLE;
  let timeout_ms: u32 = 20_000;
  let wait_rc = unsafe { WaitForSingleObject(handle, timeout_ms) };
  if wait_rc == WAIT_TIMEOUT {
    unsafe {
      let _ = TerminateProcess(handle, 1);
    }
    panic!("sandboxed child timed out after {timeout_ms}ms");
  }
  if wait_rc != WAIT_OBJECT_0 {
    unsafe {
      let _ = TerminateProcess(handle, 1);
    }
    panic!(
      "WaitForSingleObject failed (rc={wait_rc}): {}",
      std::io::Error::last_os_error()
    );
  }

  let mut exit_code: u32 = 0;
  let ok = unsafe { GetExitCodeProcess(handle, &mut exit_code) };
  assert!(ok != 0, "GetExitCodeProcess failed");
  assert_eq!(
    exit_code, 0,
    "sandboxed child exited non-zero (exit_code={exit_code}, pid={}, level={:?})",
    child.pid, child.level
  );

  // Keep the listener alive until the child is done; we never accept the connection because the
  // sandboxed child is expected to be blocked before completing the handshake.
  drop(listener);
}

#[test]
#[ignore]
fn appcontainer_network_denied_child() {
  let port: u16 = std::env::var(ENV_PORT)
    .ok()
    .and_then(|v| v.parse().ok())
    .expect("missing/invalid FASTR_TEST_WIN_SANDBOX_NETWORK_PORT");

  let token = query_current_process_token_state()
    .unwrap_or_else(|err| panic!("failed to query sandbox token state in child: {err}"));
  eprintln!("sandbox: token_state={token:?}");

  assert!(
    token.is_app_container,
    "expected sandbox child to run in an AppContainer token (no silent fallback); token_state={token:?}"
  );
  assert!(
    !token.has_internet_client_capability(),
    "SECURITY BUG: AppContainer token has internetClient capability ({INTERNET_CLIENT_CAPABILITY_SID}): {token:?}"
  );

  let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port);
  match TcpStream::connect_timeout(&addr, Duration::from_secs(2)) {
    Ok(_stream) => {
      panic!("SECURITY BUG: AppContainer sandbox allowed TCP connect to {addr}");
    }
    Err(err) => {
      assert!(
        err.kind() == io::ErrorKind::PermissionDenied || err.raw_os_error() == Some(WSAEACCES),
        "expected connect to be denied with PermissionDenied/WSAEACCES(10013), got {err:?} (raw_os_error={:?})",
        err.raw_os_error()
      );
      eprintln!(
        "connect to {addr} denied as expected: {err} (kind={:?})",
        err.kind()
      );
    }
  }
}
