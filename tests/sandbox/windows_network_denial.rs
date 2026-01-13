//! Windows sandbox regression test: AppContainer processes should not have network access.
//!
//! The preferred Windows renderer sandbox is an AppContainer with **zero capabilities** (no
//! `INTERNET_CLIENT`), which should deny outbound networking. This test validates that a sandboxed
//! child cannot establish a TCP connection to a localhost listener owned by the parent.
//!
//! If the sandbox falls back to a restricted token (or AppContainer APIs are unavailable), the test
//! prints a skip message instead of failing the suite. The restricted-token fallback is documented
//! as potentially allowing network access.

#![cfg(windows)]

use std::ffi::OsString;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::os::windows::io::AsRawHandle;
use std::time::Duration;

use fastrender::sandbox::windows::spawn_sandboxed;
use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Security::{GetTokenInformation, OpenProcessToken};
use windows_sys::Win32::System::Threading::{
  GetExitCodeProcess, GetCurrentProcess, TerminateProcess, WaitForSingleObject,
};

const ENV_PORT: &str = "FASTR_TEST_WIN_SANDBOX_NETWORK_PORT";

const TOKEN_QUERY: u32 = 0x0008;
const TOKEN_IS_APPCONTAINER: u32 = 29;

const WAIT_OBJECT_0: u32 = 0x0000_0000;
const WAIT_TIMEOUT: u32 = 0x0000_0102;

fn is_running_in_appcontainer() -> Result<bool, String> {
  let mut token: HANDLE = 0;
  let ok = unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) };
  if ok == 0 || token == 0 || token == INVALID_HANDLE_VALUE {
    return Err(format!(
      "OpenProcessToken(GetCurrentProcess) failed: {}",
      std::io::Error::last_os_error()
    ));
  }

  let mut is_appcontainer: u32 = 0;
  let mut returned: u32 = 0;
  let ok = unsafe {
    GetTokenInformation(
      token,
      TOKEN_IS_APPCONTAINER,
      &mut is_appcontainer as *mut _ as *mut _,
      std::mem::size_of::<u32>() as u32,
      &mut returned,
    )
  };
  unsafe {
    let _ = CloseHandle(token);
  }
  if ok == 0 {
    return Err(format!(
      "GetTokenInformation(TokenIsAppContainer) failed: {}",
      std::io::Error::last_os_error()
    ));
  }
  Ok(is_appcontainer != 0)
}

#[test]
fn appcontainer_denies_outbound_tcp_connect() {
  // Binding localhost can fail in some CI environments; skip with a clear message.
  let listener: TcpListener =
    match crate::common::try_bind_localhost("appcontainer_denies_outbound_tcp_connect") {
      Some(listener) => listener,
      None => return,
    };
  let port = listener
    .local_addr()
    .map(|addr| addr.port())
    .unwrap_or(0);
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
  let child = crate::common::with_env_vars(&[(ENV_PORT, &port_str)], || {
    spawn_sandboxed(&exe, &args, &[]).expect("spawn sandboxed child")
  });

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

  match is_running_in_appcontainer() {
    Ok(true) => {}
    Ok(false) => {
      eprintln!(
        "skipping AppContainer network denial check: child is not running in an AppContainer token (sandbox fallback?)"
      );
      return;
    }
    Err(err) => {
      eprintln!("skipping AppContainer network denial check: failed to query TokenIsAppContainer: {err}");
      return;
    }
  }

  let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port);
  match TcpStream::connect_timeout(&addr, Duration::from_secs(2)) {
    Ok(_stream) => {
      panic!("SECURITY BUG: AppContainer sandbox allowed TCP connect to {addr}");
    }
    Err(err) => {
      eprintln!("connect to {addr} denied as expected: {err} (kind={:?})", err.kind());
    }
  }
}

