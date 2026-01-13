#![cfg(windows)]

use std::ffi::OsString;
use std::io;
use std::net::{TcpListener, TcpStream};
use std::os::windows::io::{AsRawHandle, RawHandle};
use std::os::windows::process::ExitStatusExt;
use std::time::Duration;

use fastrender::sandbox::windows::appcontainer::appcontainer_apis;
use fastrender::sandbox::windows::{spawn_sandboxed, WindowsSandboxLevel};
use windows_sys::Win32::Foundation::{
  CloseHandle, GetHandleInformation, SetHandleInformation, ERROR_INSUFFICIENT_BUFFER, HANDLE,
  HANDLE_FLAG_INHERIT, INVALID_HANDLE_VALUE, WAIT_FAILED, WAIT_OBJECT_0, WAIT_TIMEOUT,
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

const FILE_NETWORK_TEST_NAME: &str = concat!(
  module_path!(),
  "::appcontainer_denies_filesystem_and_network"
);
const JOB_KILL_TEST_NAME: &str = concat!(
  module_path!(),
  "::job_object_kill_on_close_terminates_child"
);

/// The well-known capability SID for `internetClient`.
///
/// See: https://learn.microsoft.com/en-us/windows/security/identity-protection/access-control/security-identifiers#capability-sids
const INTERNET_CLIENT_CAPABILITY_SID: &str = "S-1-15-3-1";

#[derive(Debug)]
struct TokenState {
  is_app_container: bool,
  integrity_sid: String,
  integrity_rid: u32,
  capability_sids: Vec<String>,
}

impl TokenState {
  #[allow(dead_code)]
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

struct TokenHandle(HANDLE);

impl Drop for TokenHandle {
  fn drop(&mut self) {
    unsafe {
      let _ = CloseHandle(self.0);
    }
  }
}

fn query_current_process_token_state() -> Result<TokenState, String> {
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
  let token = TokenHandle(token);

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

fn process_handle(process: &std::os::windows::io::OwnedHandle) -> HANDLE {
  process.as_raw_handle() as HANDLE
}

fn exit_code(process: &std::os::windows::io::OwnedHandle) -> std::io::Result<u32> {
  let mut code: u32 = 0;
  // SAFETY: process handle is valid for lifetime of `OwnedHandle`.
  let ok = unsafe { GetExitCodeProcess(process_handle(process), &mut code) };
  if ok == 0 {
    return Err(std::io::Error::last_os_error());
  }
  Ok(code)
}

fn wait_for_exit_status(
  process: &std::os::windows::io::OwnedHandle,
  timeout: Duration,
) -> std::io::Result<Option<std::process::ExitStatus>> {
  let ms: u32 = timeout
    .as_millis()
    .min(u128::from(u32::MAX))
    .try_into()
    .unwrap_or(u32::MAX);
  // SAFETY: process handle is valid.
  let rc = unsafe { WaitForSingleObject(process_handle(process), ms) };
  match rc {
    WAIT_OBJECT_0 => {
      let code = exit_code(process)?;
      Ok(Some(std::process::ExitStatus::from_raw(code)))
    }
    WAIT_TIMEOUT => Ok(None),
    WAIT_FAILED => Err(std::io::Error::last_os_error()),
    other => Err(std::io::Error::new(
      std::io::ErrorKind::Other,
      format!("WaitForSingleObject returned unexpected value {other}"),
    )),
  }
}

fn wait_for_exit_or_kill(
  process: &std::os::windows::io::OwnedHandle,
  timeout: Duration,
  context: &str,
) -> std::process::ExitStatus {
  match wait_for_exit_status(process, timeout).expect("wait for sandboxed child process") {
    Some(status) => status,
    None => {
      // SAFETY: process handle is valid.
      let _ = unsafe { TerminateProcess(process_handle(process), 1) };
      panic!("timeout waiting for sandboxed child to exit ({context})");
    }
  }
}

#[test]
fn appcontainer_denies_filesystem_and_network() {
  const CHILD_ENV: &str = "FASTR_TEST_WINDOWS_RENDERER_SANDBOX_CHILD";
  const FILE_ENV: &str = "FASTR_TEST_WINDOWS_RENDERER_SANDBOX_FILE";
  const PORT_ENV: &str = "FASTR_TEST_WINDOWS_RENDERER_SANDBOX_PORT";

  if std::env::var_os(CHILD_ENV).is_some() {
    let file_path = std::env::var_os(FILE_ENV).expect("child missing file path env");
    let port_raw = std::env::var(PORT_ENV).expect("child missing port env");
    let port: u16 = port_raw.parse().expect("port should parse as u16");

    let token = query_current_process_token_state().expect("query current process token state");
    eprintln!("sandbox token state: {token:?}");
    assert!(
      token.is_app_container,
      "expected sandboxed child to run in an AppContainer token; token={token:?}"
    );
    assert!(
      !token.has_internet_client_capability(),
      "expected AppContainer token to NOT have internetClient capability ({INTERNET_CLIENT_CAPABILITY_SID}); token={token:?}"
    );

    let path = std::path::PathBuf::from(file_path);

    match std::fs::read_to_string(&path) {
      Ok(contents) => panic!(
        "expected AppContainer sandbox to deny reading {path:?}, but read {len} bytes: {contents:?}",
        len = contents.len()
      ),
      Err(err) => {
        let raw = err.raw_os_error();
        assert!(
          matches!(
            err.kind(),
            std::io::ErrorKind::PermissionDenied | std::io::ErrorKind::NotFound
          ) || matches!(raw, Some(2 | 3 | 5)),
          "expected read_to_string({path:?}) to fail with PermissionDenied/NotFound (common for AppContainer) or ERROR_ACCESS_DENIED(5)/PATH_NOT_FOUND(3)/FILE_NOT_FOUND(2), got {err:?} (raw_os_error={raw:?})"
        );
      }
    }

    if port != 0 {
      let addr = std::net::SocketAddr::from((std::net::Ipv4Addr::LOCALHOST, port));
      match TcpStream::connect_timeout(&addr, Duration::from_secs(2)) {
        Ok(_) => {
          panic!("expected AppContainer sandbox to deny TcpStream::connect to 127.0.0.1:{port}")
        }
        Err(err) => {
          assert!(
            err.kind() == std::io::ErrorKind::PermissionDenied || err.raw_os_error() == Some(10013),
            "expected connect to fail with PermissionDenied/WSAEACCES(10013), got {err:?}"
          );
        }
      }
    } else {
      eprintln!(
        "skipping localhost connect assertion in sandbox child: parent could not bind localhost"
      );
    }

    return;
  }

  if !crate::common::windows_sandbox::require_full_windows_sandbox(
    "appcontainer_denies_filesystem_and_network",
  ) {
    return;
  }

  // Serialize network-heavy tests to keep CI deterministic. Take this lock first to avoid lock-order
  // inversions with helpers that also take the global env lock.
  let _net_guard = crate::common::net_test_lock();

  // This test is AppContainer-specific; skip on older Windows versions where AppContainer APIs are
  // unavailable and the sandbox falls back to restricted-token mode.
  if appcontainer_apis().is_err() {
    eprintln!(
      "skipping AppContainer filesystem/network denial test: AppContainer APIs are unavailable on this OS"
    );
    return;
  }

  // Ensure developer environment overrides don't silently change test semantics.
  let _sandbox_env_guard = crate::common::EnvVarsGuard::remove(&[
    "FASTR_DISABLE_RENDERER_SANDBOX",
    "FASTR_WINDOWS_RENDERER_SANDBOX",
    "FASTR_ALLOW_UNSANDBOXED_RENDERER",
    "FASTR_DISABLE_WIN_MITIGATIONS",
    "FASTR_WINDOWS_SANDBOX_INHERIT_ENV",
  ]);

  let temp_dir = tempfile::tempdir().expect("create temp dir");
  let file_path = temp_dir.path().join("fastrender_windows_sandbox_probe.txt");
  std::fs::write(&file_path, "fastrender sandbox probe").expect("write probe file");
  let parent_contents =
    std::fs::read_to_string(&file_path).expect("parent should be able to read probe file");
  assert_eq!(parent_contents, "fastrender sandbox probe");

  let listener = crate::common::try_bind_localhost(
    "appcontainer_denies_filesystem_and_network (network portion)",
  );
  let port = listener
    .as_ref()
    .and_then(|listener| listener.local_addr().ok().map(|addr| addr.port()))
    .unwrap_or(0);

  let exe = std::env::current_exe().expect("current test executable path");
  let args = vec![
    OsString::from("--exact"),
    OsString::from(FILE_NETWORK_TEST_NAME),
    OsString::from("--nocapture"),
  ];
  let child = {
    let _child_env = crate::common::EnvVarGuard::set(CHILD_ENV, "1");
    let _file_env = crate::common::EnvVarGuard::set(FILE_ENV, file_path.as_os_str());
    let _port_env = crate::common::EnvVarGuard::set(PORT_ENV, port.to_string());
    let (inherit_handles, _inherit_guard) = collect_stdio_handles_for_inheritance();
    let child =
      spawn_sandboxed(&exe, &args, &inherit_handles).expect("spawn sandboxed child process");
    assert_eq!(
      child.level,
      WindowsSandboxLevel::AppContainer,
      "expected AppContainer sandboxing to succeed (not fall back)"
    );
    child
  };

  // Keep the listener alive for the duration of the child probe so `ECONNREFUSED` isn't a false
  // positive.
  let _listener_guard = listener;

  let status = wait_for_exit_or_kill(
    &child.process,
    Duration::from_secs(10),
    "appcontainer probe",
  );
  assert!(
    status.success(),
    "sandboxed probe child should exit successfully (status={status:?})"
  );
}

#[test]
fn job_object_kill_on_close_terminates_child() {
  const CHILD_ENV: &str = "FASTR_TEST_WINDOWS_RENDERER_JOB_CHILD";

  if std::env::var_os(CHILD_ENV).is_some() {
    loop {
      std::thread::sleep(Duration::from_secs(1));
    }
  }

  if !crate::common::windows_sandbox::require_full_windows_sandbox(
    "job_object_kill_on_close_terminates_child",
  ) {
    return;
  }

  // Ensure developer environment overrides don't silently change test semantics.
  let _env_guard = crate::common::EnvVarsGuard::remove(&[
    "FASTR_DISABLE_RENDERER_SANDBOX",
    "FASTR_WINDOWS_RENDERER_SANDBOX",
    "FASTR_ALLOW_UNSANDBOXED_RENDERER",
    "FASTR_DISABLE_WIN_MITIGATIONS",
  ]);

  let exe = std::env::current_exe().expect("current test executable path");
  let args = vec![
    OsString::from("--exact"),
    OsString::from(JOB_KILL_TEST_NAME),
    OsString::from("--nocapture"),
  ];
  let fastrender::sandbox::windows::SandboxedChild { process, job, .. } = {
    let _child_env = crate::common::EnvVarGuard::set(CHILD_ENV, "1");
    spawn_sandboxed(&exe, &args, &[]).expect("spawn sandboxed child process")
  };

  // Ensure the child is actually running (otherwise a crash could make this test pass trivially).
  std::thread::sleep(Duration::from_millis(200));
  assert!(
    wait_for_exit_status(&process, Duration::from_millis(0))
      .expect("poll sandboxed child")
      .is_none(),
    "expected child to still be running before job is closed"
  );

  drop(job);

  let status = match wait_for_exit_status(&process, Duration::from_secs(3))
    .expect("wait for child to terminate after closing job")
  {
    Some(status) => status,
    None => {
      let _ = unsafe { TerminateProcess(process_handle(&process), 1) };
      panic!("expected JobObject kill-on-close to terminate child within timeout");
    }
  };

  assert!(
    !status.success(),
    "child should not exit cleanly when terminated by JobObject (status={status:?})"
  );
}
