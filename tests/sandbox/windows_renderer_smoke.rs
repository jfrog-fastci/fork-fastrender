//! Windows sandbox compatibility smoke test: ensure a sandboxed child (preferably AppContainer)
//! can initialize FastRender and render a minimal HTML page.
//!
//! Motivation: AppContainer can restrict filesystem access to system fonts (e.g. `C:\Windows\Fonts`)
//! which FastRender relies on for text shaping. This test gives an early signal when the sandbox
//! policy makes the renderer unusable.
//!
//! Implementation notes:
//! - Uses the production Windows sandbox launcher (`fastrender::sandbox::windows::spawn_sandboxed`)
//!   so we validate the same code path used by future multiprocess renderer spawning.
//! - The child test is marked `#[ignore]` and is executed via `--ignored --exact ...` inside the
//!   sandboxed process.

#![cfg(windows)]

use std::error::Error;
use std::ffi::OsString;
use std::os::windows::io::{AsRawHandle, RawHandle};

use fastrender::sandbox::windows::{spawn_sandboxed, WindowsSandboxLevel};
use windows_sys::Win32::Foundation::{
  CloseHandle, SetHandleInformation, HANDLE, HANDLE_FLAG_INHERIT, INVALID_HANDLE_VALUE,
};
use windows_sys::Win32::Security::{GetTokenInformation, OpenProcessToken};
use windows_sys::Win32::System::Console::{
  GetStdHandle, STD_ERROR_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE,
};
use windows_sys::Win32::System::Threading::{
  GetExitCodeProcess, TerminateProcess, WaitForSingleObject,
};

// WaitForSingleObject return codes.
const WAIT_OBJECT_0: u32 = 0;
const WAIT_TIMEOUT: u32 = 0x0000_0102;

const TOKEN_QUERY: u32 = 0x0008;
const TOKEN_IS_APPCONTAINER: u32 = 29;

const CHILD_TIMEOUT_MS: u32 = 60_000;

fn format_error_chain(err: &(dyn Error)) -> String {
  let mut out = String::new();
  out.push_str(&format!("{err}"));
  let mut source = err.source();
  while let Some(src) = source {
    out.push_str(&format!("\n  caused by: {src}"));
    source = src.source();
  }
  out
}

fn is_running_in_appcontainer() -> Result<bool, String> {
  // GetCurrentProcess() returns a pseudo-handle with value -1.
  let current_process: HANDLE = (-1isize) as HANDLE;
  let mut token: HANDLE = 0;
  let ok = unsafe { OpenProcessToken(current_process, TOKEN_QUERY, &mut token) };
  if ok == 0 {
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

fn wait_process(handle: HANDLE, timeout_ms: u32) -> Result<u32, String> {
  let wait = unsafe { WaitForSingleObject(handle, timeout_ms) };
  if wait == WAIT_TIMEOUT {
    unsafe {
      // Best-effort kill so the test process doesn't hang indefinitely.
      TerminateProcess(handle, 1);
    }
    return Err(format!(
      "child process timed out after {timeout_ms}ms (terminated)"
    ));
  }
  if wait != WAIT_OBJECT_0 {
    return Err(format!(
      "WaitForSingleObject failed (code={wait}): {}",
      std::io::Error::last_os_error()
    ));
  }

  let mut exit_code: u32 = 0;
  let ok = unsafe { GetExitCodeProcess(handle, &mut exit_code) };
  if ok == 0 {
    return Err(format!(
      "GetExitCodeProcess failed: {}",
      std::io::Error::last_os_error()
    ));
  }

  Ok(exit_code)
}

fn collect_stdio_handles_for_inheritance() -> Vec<RawHandle> {
  // Limit handle inheritance to standard handles so we don't leak privileged handles into the
  // sandboxed process.
  let std_in = unsafe { GetStdHandle(STD_INPUT_HANDLE) };
  let std_out = unsafe { GetStdHandle(STD_OUTPUT_HANDLE) };
  let std_err = unsafe { GetStdHandle(STD_ERROR_HANDLE) };

  let mut inherit = Vec::new();
  for h in [std_in, std_out, std_err] {
    if h == 0 || h == INVALID_HANDLE_VALUE {
      continue;
    }
    // Ensure the handle is inheritable so the sandbox spawn can forward it explicitly.
    let _ = unsafe { SetHandleInformation(h, HANDLE_FLAG_INHERIT, HANDLE_FLAG_INHERIT) };
    inherit.push(h as RawHandle);
  }
  inherit
}

#[test]
fn appcontainer_renderer_can_render_minimal_html() {
  let exe = std::env::current_exe().expect("current test exe path");
  let test_name = "sandbox::windows_renderer_smoke::appcontainer_renderer_smoke_child";

  let args = vec![
    OsString::from("--ignored"),
    OsString::from("--exact"),
    OsString::from(test_name),
    OsString::from("--nocapture"),
  ];

  let inherit = collect_stdio_handles_for_inheritance();

  let child = spawn_sandboxed(&exe, &args, &inherit).expect("spawn sandboxed child");
  let handle = child.process.as_raw_handle() as HANDLE;
  let exit_code = wait_process(handle, CHILD_TIMEOUT_MS).expect("wait for sandboxed child");

  assert_eq!(
    exit_code, 0,
    "sandboxed child exited with code {exit_code} (sandbox_level={:?})",
    child.level
  );

  if child.level != WindowsSandboxLevel::AppContainer {
    eprintln!(
      "skipping AppContainer-specific assertion: sandbox spawn fell back to {:?}",
      child.level
    );
  }
}

#[test]
#[ignore]
fn appcontainer_renderer_smoke_child() {
  match is_running_in_appcontainer() {
    Ok(true) => eprintln!("sandbox: running in AppContainer token"),
    Ok(false) => eprintln!("sandbox: NOT running in AppContainer token (spawn fallback?)"),
    Err(err) => eprintln!("sandbox: failed to query TokenIsAppContainer: {err}"),
  }

  if let Err(err) = renderer_smoke_child_inner() {
    let chain = format_error_chain(&err);
    panic!("Sandboxed child failed to initialize FastRender and render minimal HTML:\n{chain}");
  }
}

fn renderer_smoke_child_inner() -> fastrender::Result<()> {
  use fastrender::image_output::{encode_image, OutputFormat};
  use fastrender::FastRender;

  let mut renderer = FastRender::new()?;
  let pixmap = renderer.render_html("<!doctype html><p>Hello</p>", 256, 128)?;
  assert_eq!(pixmap.width(), 256);
  assert_eq!(pixmap.height(), 128);

  let png = encode_image(&pixmap, OutputFormat::Png)?;
  assert!(
    png.starts_with(b"\x89PNG\r\n\x1a\n"),
    "expected PNG signature, got {:?}",
    png.get(0..8)
  );
  Ok(())
}
