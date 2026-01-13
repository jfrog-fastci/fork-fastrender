//! Windows sandbox compatibility smoke test: ensure an AppContainer-sandboxed renderer can
//! initialize FastRender and render a minimal HTML page.
//!
//! Motivation: AppContainer can restrict filesystem access to system fonts (e.g. `C:\Windows\Fonts`)
//! which FastRender relies on for text shaping. This test gives an early signal when the sandbox
//! policy makes the renderer unusable.

#![cfg(windows)]

use std::error::Error;
use std::ffi::OsStr;
use std::os::windows::ffi::OsStrExt;
use std::path::PathBuf;
use std::ptr::{null, null_mut};

use windows_sys::Win32::Foundation::{
  CloseHandle, GetLastError, SetHandleInformation, HANDLE, HANDLE_FLAG_INHERIT,
  INVALID_HANDLE_VALUE,
};
use windows_sys::Win32::Security::Authentication::Identity::{
  CreateAppContainerProfile, DeriveAppContainerSidFromAppContainerName,
};
use windows_sys::Win32::Security::{
  FreeSid, GetTokenInformation, OpenProcessToken, SECURITY_CAPABILITIES,
};
use windows_sys::Win32::System::Console::{
  GetStdHandle, STD_ERROR_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE,
};
use windows_sys::Win32::System::Threading::{
  CreateProcessW, DeleteProcThreadAttributeList, GetExitCodeProcess,
  InitializeProcThreadAttributeList, TerminateProcess, UpdateProcThreadAttribute,
  WaitForSingleObject, PROCESS_INFORMATION, PROC_THREAD_ATTRIBUTE_LIST, STARTUPINFOEXW,
};

// `PROC_THREAD_ATTRIBUTE_*` values are stable ABI constants from winbase.h, derived via:
//   ProcThreadAttributeValue(Number, Thread, Input, Additive)
// We define them here to avoid relying on a specific `windows-sys` version exporting them.
const PROC_THREAD_ATTRIBUTE_HANDLE_LIST: usize = 0x0002_0002;
const PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES: usize = 0x0002_0009;

const STARTF_USESTDHANDLES: u32 = 0x0000_0100;
const EXTENDED_STARTUPINFO_PRESENT: u32 = 0x0008_0000;

const WAIT_OBJECT_0: u32 = 0;
const WAIT_TIMEOUT: u32 = 0x0000_0102;

const TOKEN_QUERY: u32 = 0x0008;
const TOKEN_IS_APPCONTAINER: u32 = 29;

const CHILD_TIMEOUT_MS: u32 = 60_000;

const APP_CONTAINER_NAME: &str = "fastrender.sandbox.renderer-smoke-test";

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

fn system32_dir() -> String {
  // Use a working directory that is expected to be readable by an AppContainer process.
  let root = std::env::var("SystemRoot").unwrap_or_else(|_| "C:\\Windows".to_string());
  format!("{root}\\System32")
}

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

fn ensure_running_in_appcontainer() -> Result<(), String> {
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
  unsafe { CloseHandle(token) };
  if ok == 0 {
    return Err(format!(
      "GetTokenInformation(TokenIsAppContainer) failed: {}",
      std::io::Error::last_os_error()
    ));
  }

  if is_appcontainer == 0 {
    return Err("process token is not marked as AppContainer (sandbox fallback?)".to_string());
  }
  Ok(())
}

fn wait_for_process(pi: &PROCESS_INFORMATION, timeout_ms: u32) -> Result<u32, String> {
  let wait = unsafe { WaitForSingleObject(pi.hProcess, timeout_ms) };
  if wait == WAIT_TIMEOUT {
    unsafe {
      // Best-effort kill so the test process doesn't hang indefinitely.
      TerminateProcess(pi.hProcess, 1);
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
  let ok = unsafe { GetExitCodeProcess(pi.hProcess, &mut exit_code) };
  if ok == 0 {
    return Err(format!(
      "GetExitCodeProcess failed: {}",
      std::io::Error::last_os_error()
    ));
  }
  Ok(exit_code)
}

fn spawn_appcontainer_child(test_filter: &str) -> Result<u32, String> {
  let exe: PathBuf = std::env::current_exe().map_err(|e| format!("current_exe failed: {e}"))?;

  // libtest expects: [--ignored] <filter> --exact [--nocapture]
  let cmdline = format!(
    "\"{}\" --ignored --exact {} --nocapture",
    exe.display(),
    test_filter
  );
  let mut cmdline = wide_null_str(&cmdline);

  let current_dir = system32_dir();
  let current_dir = wide_null_str(&current_dir);

  unsafe {
    // Ensure the AppContainer profile exists (best-effort). If AppContainer isn't supported (older
    // Windows), treat this test as a skip rather than a failure.
    let mut sid = null_mut();
    let name_w = wide_null_str(APP_CONTAINER_NAME);
    let display_w = wide_null_str("FastRender sandbox renderer smoke test");
    let description_w = wide_null_str("FastRender sandbox renderer smoke test profile");

    let hr = CreateAppContainerProfile(
      name_w.as_ptr(),
      display_w.as_ptr(),
      description_w.as_ptr(),
      null_mut(),
      0,
      &mut sid,
    );

    // `CreateAppContainerProfile` returns HRESULT_FROM_WIN32(ERROR_ALREADY_EXISTS) when the profile
    // already exists. In that case (or any other failure), derive the SID from the name.
    if hr != 0 {
      sid = null_mut();
      let hr = DeriveAppContainerSidFromAppContainerName(name_w.as_ptr(), &mut sid);
      if hr != 0 {
        eprintln!(
          "skipping Windows AppContainer renderer smoke test: DeriveAppContainerSidFromAppContainerName failed (hr=0x{hr:08X})"
        );
        return Ok(0);
      }
    } else if !sid.is_null() {
      // `CreateAppContainerProfile` returns a freshly-allocated SID. We only need the profile and
      // will derive the SID again (consistent across processes), so free this one immediately.
      FreeSid(sid);
      sid = null_mut();
      let hr = DeriveAppContainerSidFromAppContainerName(name_w.as_ptr(), &mut sid);
      if hr != 0 {
        eprintln!(
          "skipping Windows AppContainer renderer smoke test: DeriveAppContainerSidFromAppContainerName failed after profile creation (hr=0x{hr:08X})"
        );
        return Ok(0);
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
    let _sid_guard = SidGuard(sid);

    let mut security_caps = SECURITY_CAPABILITIES {
      AppContainerSid: sid,
      Capabilities: null_mut(),
      CapabilityCount: 0,
      Reserved: 0,
    };

    // Limit handle inheritance to standard handles so we don't leak any privileged handles into
    // the sandboxed process.
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
    InitializeProcThreadAttributeList(null_mut(), attribute_count, 0, &mut attr_size);
    if attr_size == 0 {
      return Err("InitializeProcThreadAttributeList returned size=0".to_string());
    }

    // Ensure alignment for `PROC_THREAD_ATTRIBUTE_LIST` by allocating as `usize`.
    let units = (attr_size + std::mem::size_of::<usize>() - 1) / std::mem::size_of::<usize>();
    let mut attr_buf: Vec<usize> = vec![0; units];
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
      &mut security_caps as *mut _ as *mut _,
      std::mem::size_of::<SECURITY_CAPABILITIES>(),
      null_mut(),
      null_mut(),
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
        handles.as_mut_ptr() as *mut _,
        handles.len() * std::mem::size_of::<HANDLE>(),
        null_mut(),
        null_mut(),
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
    let ok = CreateProcessW(
      null(),
      cmdline.as_mut_ptr(),
      null(),
      null(),
      1,
      EXTENDED_STARTUPINFO_PRESENT,
      null(),
      current_dir.as_ptr(),
      &mut si.StartupInfo,
      &mut pi,
    );
    if ok == 0 {
      return Err(format!(
        "CreateProcessW (AppContainer) failed: {}",
        std::io::Error::last_os_error()
      ));
    }

    let exit_code = wait_for_process(&pi, CHILD_TIMEOUT_MS)?;
    CloseHandle(pi.hThread);
    CloseHandle(pi.hProcess);
    Ok(exit_code)
  }
}

#[test]
fn appcontainer_renderer_can_render_minimal_html() {
  let test_filter = "sandbox::windows_renderer_smoke::appcontainer_renderer_smoke_child";
  let exit = spawn_appcontainer_child(test_filter).expect("spawn AppContainer child");
  assert_eq!(exit, 0, "AppContainer child exited with code {exit}");
}

#[test]
#[ignore]
fn appcontainer_renderer_smoke_child() {
  ensure_running_in_appcontainer().expect("child should be running in AppContainer");

  if let Err(err) = renderer_smoke_child_inner() {
    let chain = format_error_chain(&err);
    panic!("AppContainer child failed to initialize FastRender and render minimal HTML:\n{chain}");
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
