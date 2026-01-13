#![cfg(windows)]

mod common;

use std::ffi::c_void;
use std::ffi::OsString;
use std::io;
use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::os::windows::process::ExitStatusExt;
use std::time::Duration;

use win_sandbox::mitigations;
use win_sandbox::RendererSandbox;
use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, HANDLE};
use windows_sys::Win32::Security::{
  GetTokenInformation, TokenCapabilities, TokenIsAppContainer, TOKEN_QUERY,
};
use windows_sys::Win32::System::Threading::{
  GetCurrentProcess, GetExitCodeProcess, OpenProcessToken, WaitForSingleObject,
};

const CHILD_ENV: &str = "FASTR_TEST_WIN_APPCONTAINER_NETWORK_CHILD";
const PORT_ENV: &str = "FASTR_TEST_WIN_APPCONTAINER_NETWORK_PORT";

const WSAEACCES: i32 = 10013;

const WAIT_OBJECT_0: u32 = 0x0000_0000;
const WAIT_TIMEOUT: u32 = 0x0000_0102;

struct HandleGuard(HANDLE);

impl Drop for HandleGuard {
  fn drop(&mut self) {
    unsafe {
      if !self.0.is_null() {
        CloseHandle(self.0);
      }
    }
  }
}

fn wait_process(handle: HANDLE, timeout_ms: u32) -> io::Result<std::process::ExitStatus> {
  let wait = unsafe { WaitForSingleObject(handle, timeout_ms) };
  if wait == WAIT_TIMEOUT {
    return Err(io::Error::new(
      io::ErrorKind::TimedOut,
      "sandboxed child timed out",
    ));
  }
  if wait != WAIT_OBJECT_0 {
    return Err(io::Error::last_os_error());
  }

  let mut exit_code: u32 = 0;
  let ok = unsafe { GetExitCodeProcess(handle, &mut exit_code) };
  if ok == 0 {
    return Err(io::Error::last_os_error());
  }
  Ok(std::process::ExitStatus::from_raw(exit_code))
}

#[repr(C)]
struct SidAndAttributes {
  sid: windows_sys::Win32::Security::PSID,
  attributes: u32,
}

#[repr(C)]
struct TokenGroups {
  group_count: u32,
  groups: [SidAndAttributes; 1],
}

unsafe fn wide_ptr_len(mut ptr: *const u16) -> usize {
  let mut len = 0;
  while !ptr.is_null() && *ptr != 0 {
    len += 1;
    ptr = ptr.add(1);
  }
  len
}

unsafe fn token_capability_sids(token: HANDLE) -> io::Result<Vec<String>> {
  let mut required: u32 = 0;
  let ok = GetTokenInformation(
    token,
    TokenCapabilities,
    std::ptr::null_mut(),
    0,
    &mut required,
  );

  if ok == 0 && required == 0 {
    return Err(io::Error::from_raw_os_error(GetLastError() as i32));
  }

  let mut buf = vec![0u8; required as usize];
  let ok = GetTokenInformation(
    token,
    TokenCapabilities,
    buf.as_mut_ptr().cast::<c_void>(),
    required,
    &mut required,
  );
  if ok == 0 {
    return Err(io::Error::last_os_error());
  }

  let groups = buf.as_ptr().cast::<TokenGroups>();
  let count = (*groups).group_count as usize;
  let ptr = std::ptr::addr_of!((*groups).groups).cast::<SidAndAttributes>();
  let slice = std::slice::from_raw_parts(ptr, count);

  let mut out = Vec::with_capacity(count);
  for entry in slice {
    let mut sid_str: *mut u16 = std::ptr::null_mut();
    let ok =
      windows_sys::Win32::Security::Authorization::ConvertSidToStringSidW(entry.sid, &mut sid_str);
    if ok == 0 || sid_str.is_null() {
      return Err(io::Error::last_os_error());
    }
    let len = wide_ptr_len(sid_str);
    let wide = std::slice::from_raw_parts(sid_str, len);
    out.push(String::from_utf16_lossy(wide));
    windows_sys::Win32::Foundation::LocalFree(sid_str.cast());
  }
  Ok(out)
}

#[test]
fn appcontainer_denies_network() {
  let is_child = std::env::var_os(CHILD_ENV).is_some();
  if is_child {
    // -------------------------------------------------------------------------
    // Child path: validate the token contains no network capabilities + connect fails.
    // -------------------------------------------------------------------------
    mitigations::verify_renderer_mitigations_current_process()
      .expect("expected renderer mitigations to be active in sandboxed child");
    unsafe {
      let mut token: HANDLE = std::ptr::null_mut();
      let ok = OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token);
      assert_ne!(
        ok,
        0,
        "OpenProcessToken failed: {}",
        io::Error::last_os_error()
      );
      assert!(!token.is_null(), "OpenProcessToken returned null handle");
      let token_guard = HandleGuard(token);

      // Ensure we are actually running inside an AppContainer token (not a fallback sandbox).
      let mut is_appcontainer: u32 = 0;
      let mut returned: u32 = 0;
      let ok = GetTokenInformation(
        token_guard.0,
        TokenIsAppContainer,
        (&mut is_appcontainer as *mut u32).cast::<c_void>(),
        std::mem::size_of::<u32>() as u32,
        &mut returned,
      );
      assert_ne!(
        ok,
        0,
        "GetTokenInformation(TokenIsAppContainer) failed: {}",
        io::Error::last_os_error()
      );
      assert_ne!(
        is_appcontainer, 0,
        "child token is not an AppContainer token (sandbox fallback?)"
      );

      let caps = token_capability_sids(token_guard.0).expect("query TokenCapabilities");
      assert!(
        caps.is_empty(),
        "expected TokenCapabilities to be empty for a no-capabilities AppContainer; got {caps:?}"
      );
    }

    let port: u16 = std::env::var(PORT_ENV)
      .expect("child process missing sandbox port env var")
      .parse()
      .expect("parse sandbox port env var");
    let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
    let timeout = Duration::from_secs(2);
    match TcpStream::connect_timeout(&addr, timeout) {
      Ok(_) => panic!(
        "unexpectedly connected to {addr} from AppContainer with no capabilities (network escape?)"
      ),
      Err(err) => {
        assert_eq!(
          err.raw_os_error(),
          Some(WSAEACCES),
          "expected connect to fail with WSAEACCES (10013) under no-capabilities AppContainer; got {err:?}"
        );
      }
    }

    return;
  }

  // ---------------------------------------------------------------------------
  // Parent path: spawn this test under the win-sandbox renderer AppContainer sandbox.
  // ---------------------------------------------------------------------------
  if !common::require_appcontainer_profile("win-sandbox AppContainer network denial test") {
    return;
  }

  static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
  let _guard = ENV_LOCK.lock().unwrap();

  let listener = match TcpListener::bind("127.0.0.1:0") {
    Ok(listener) => listener,
    Err(err)
      if matches!(
        err.kind(),
        io::ErrorKind::PermissionDenied | io::ErrorKind::AddrNotAvailable
      ) =>
    {
      eprintln!(
        "skipping win-sandbox AppContainer network denial test: cannot bind localhost: {err}"
      );
      return;
    }
    Err(err) => panic!("bind test TCP listener: {err}"),
  };
  let port = listener
    .local_addr()
    .expect("listener local addr")
    .port()
    .to_string();

  let prev_child = std::env::var_os(CHILD_ENV);
  std::env::set_var(CHILD_ENV, "1");
  let prev_port = std::env::var_os(PORT_ENV);
  std::env::set_var(PORT_ENV, &port);
  let prev_threads = std::env::var_os("RUST_TEST_THREADS");
  std::env::set_var("RUST_TEST_THREADS", "1");

  let exe = std::env::current_exe().expect("current test exe path");
  let args = vec![
    OsString::from("--exact"),
    OsString::from("appcontainer_denies_network"),
    OsString::from("--nocapture"),
  ];

  let sandbox = RendererSandbox::appcontainer_no_capabilities();
  let child = sandbox.spawn(&exe, &args).expect("spawn sandboxed child");

  let handle = child.process.as_raw();
  let status = wait_process(handle, 20_000).expect("wait for sandboxed child");
  assert!(status.success(), "sandboxed child failed (exit={status})");
  drop(listener);

  match prev_child {
    Some(value) => std::env::set_var(CHILD_ENV, value),
    None => std::env::remove_var(CHILD_ENV),
  }
  match prev_port {
    Some(value) => std::env::set_var(PORT_ENV, value),
    None => std::env::remove_var(PORT_ENV),
  }
  match prev_threads {
    Some(value) => std::env::set_var("RUST_TEST_THREADS", value),
    None => std::env::remove_var("RUST_TEST_THREADS"),
  }
}
