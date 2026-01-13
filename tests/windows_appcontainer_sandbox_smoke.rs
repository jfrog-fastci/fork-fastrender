#![cfg(windows)]

use std::ffi::{OsStr, OsString};
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::ptr::{null, null_mut};

use windows_sys::Win32::Foundation::{
  CloseHandle, ERROR_ALREADY_EXISTS, ERROR_CALL_NOT_IMPLEMENTED, ERROR_NOT_SUPPORTED, HANDLE,
  WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use windows_sys::Win32::System::Threading::{
  CreateProcessW, DeleteProcThreadAttributeList, GetExitCodeProcess,
  InitializeProcThreadAttributeList, TerminateProcess, UpdateProcThreadAttribute,
  WaitForSingleObject, EXTENDED_STARTUPINFO_PRESENT, PROCESS_INFORMATION,
  PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES, STARTUPINFOEXW,
};

// `windows-sys` does not currently expose all AppContainer/UserEnv + token APIs at stable
// namespace paths across versions. Keep the integration test self-contained with a minimal FFI
// surface.
#[repr(C)]
struct SidAndAttributes {
  sid: *mut std::ffi::c_void,
  attributes: u32,
}

#[repr(C)]
struct SecurityCapabilities {
  app_container_sid: *mut std::ffi::c_void,
  capabilities: *mut SidAndAttributes,
  capability_count: u32,
  reserved: u32,
}

#[link(name = "userenv")]
extern "system" {
  fn CreateAppContainerProfile(
    app_container_name: *const u16,
    display_name: *const u16,
    description: *const u16,
    capabilities: *const SidAndAttributes,
    capability_count: u32,
    app_container_sid: *mut *mut std::ffi::c_void,
  ) -> i32;

  fn DeleteAppContainerProfile(app_container_name: *const u16) -> i32;
}

#[link(name = "advapi32")]
extern "system" {
  fn FreeSid(sid: *mut std::ffi::c_void) -> *mut std::ffi::c_void;
  fn OpenProcessToken(process: HANDLE, desired_access: u32, token: *mut HANDLE) -> i32;
  fn GetTokenInformation(
    token: HANDLE,
    token_information_class: u32,
    token_information: *mut std::ffi::c_void,
    token_information_length: u32,
    return_length: *mut u32,
  ) -> i32;
}

const TOKEN_QUERY: u32 = 0x0008;
const TOKEN_IS_APPCONTAINER: u32 = 29;
const CHILD_TEST_NAME: &str = "appcontainer_renderer_smoke_child";
const CHILD_TIMEOUT_MS: u32 = 60_000;

fn to_wide_null(s: &OsStr) -> Vec<u16> {
  let mut wide: Vec<u16> = s.encode_wide().collect();
  wide.push(0);
  wide
}

fn to_wide_null_str(s: &str) -> Vec<u16> {
  to_wide_null(OsStr::new(s))
}

fn os_string_from_wide_null(wide: &[u16]) -> OsString {
  let trimmed = wide.strip_suffix(&[0]).unwrap_or(wide);
  OsString::from_wide(trimmed)
}

fn hresult_from_win32(err: u32) -> i32 {
  if err == 0 {
    0
  } else {
    // Matches HRESULT_FROM_WIN32 macro.
    (0x8007_0000u32 | (err & 0x0000_FFFF)) as i32
  }
}

fn hresult_code(hr: i32) -> u32 {
  // Low 16 bits for HRESULT_FROM_WIN32 failures.
  (hr as u32) & 0xFFFF
}

#[derive(Debug)]
enum AppContainerProfileError {
  NotSupported(String),
  Failed(String),
}

#[derive(Debug)]
struct AppContainerProfile {
  name_wide: Vec<u16>,
  sid: *mut std::ffi::c_void,
}

impl AppContainerProfile {
  fn create_unique() -> Result<Self, AppContainerProfileError> {
    let now = std::time::SystemTime::now()
      .duration_since(std::time::UNIX_EPOCH)
      .map_err(|e| {
        AppContainerProfileError::Failed(format!("SystemTime before UNIX_EPOCH: {e}"))
      })?;
    let name = format!(
      "FastRenderSandboxSmokeTest_{}_{}",
      std::process::id(),
      now.as_nanos()
    );
    let name_wide = to_wide_null_str(&name);
    let mut sid: *mut std::ffi::c_void = null_mut();

    // CreateAppContainerProfile returns an HRESULT.
    let hr = unsafe {
      CreateAppContainerProfile(
        name_wide.as_ptr(),
        name_wide.as_ptr(),
        name_wide.as_ptr(),
        null(),
        0,
        &mut sid,
      )
    };

    if hr == 0 {
      if sid.is_null() {
        return Err(AppContainerProfileError::Failed(
          "CreateAppContainerProfile returned success but sid was null".to_string(),
        ));
      }
      return Ok(Self { name_wide, sid });
    }

    let win32 = hresult_code(hr);
    if hr == hresult_from_win32(ERROR_ALREADY_EXISTS) {
      return Err(AppContainerProfileError::Failed(format!(
        "CreateAppContainerProfile reported ERROR_ALREADY_EXISTS for generated name '{}'",
        os_string_from_wide_null(&name_wide).to_string_lossy()
      )));
    }

    // E_NOTIMPL (0x80004001) is returned on older Windows builds that don't support AppContainer.
    if hr == 0x8000_4001u32 as i32
      || hr == hresult_from_win32(ERROR_CALL_NOT_IMPLEMENTED)
      || hr == hresult_from_win32(ERROR_NOT_SUPPORTED)
    {
      return Err(AppContainerProfileError::NotSupported(
        "AppContainer APIs not available on this Windows build".to_string(),
      ));
    }

    Err(AppContainerProfileError::Failed(format!(
      "CreateAppContainerProfile failed (HRESULT=0x{hr:08X}, win32={win32}) for '{}'",
      os_string_from_wide_null(&name_wide).to_string_lossy()
    )))
  }

  fn as_security_capabilities(&self) -> SecurityCapabilities {
    SecurityCapabilities {
      app_container_sid: self.sid,
      capabilities: null_mut(),
      capability_count: 0,
      reserved: 0,
    }
  }
}

impl Drop for AppContainerProfile {
  fn drop(&mut self) {
    unsafe {
      // Best-effort cleanup. Ignore failures; the profile is ephemeral.
      let _ = DeleteAppContainerProfile(self.name_wide.as_ptr());
      if !self.sid.is_null() {
        FreeSid(self.sid);
        self.sid = null_mut();
      }
    }
  }
}

fn spawn_appcontainer_process(
  profile: &AppContainerProfile,
  exe: &OsStr,
  args: &[&str],
) -> Result<PROCESS_INFORMATION, String> {
  let security_capabilities = profile.as_security_capabilities();

  // Attribute list setup.
  let mut attr_list_size: usize = 0;
  unsafe {
    InitializeProcThreadAttributeList(null_mut(), 1, 0, &mut attr_list_size);
  }
  if attr_list_size == 0 {
    return Err("InitializeProcThreadAttributeList returned size=0".to_string());
  }

  let mut attr_storage = vec![0u8; attr_list_size];
  let attr_list_ptr = attr_storage.as_mut_ptr() as *mut _;
  let ok = unsafe { InitializeProcThreadAttributeList(attr_list_ptr, 1, 0, &mut attr_list_size) };
  if ok == 0 {
    return Err(format!(
      "InitializeProcThreadAttributeList failed: {}",
      std::io::Error::last_os_error()
    ));
  }

  let ok = unsafe {
    UpdateProcThreadAttribute(
      attr_list_ptr,
      0,
      PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES as usize,
      &security_capabilities as *const _ as *mut _,
      std::mem::size_of::<SecurityCapabilities>(),
      null_mut(),
      null_mut(),
    )
  };
  if ok == 0 {
    unsafe { DeleteProcThreadAttributeList(attr_list_ptr) };
    return Err(format!(
      "UpdateProcThreadAttribute(PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES) failed: {}",
      std::io::Error::last_os_error()
    ));
  }

  let mut startup: STARTUPINFOEXW = unsafe { std::mem::zeroed() };
  startup.StartupInfo.cb = std::mem::size_of::<STARTUPINFOEXW>() as u32;
  startup.lpAttributeList = attr_list_ptr;

  // Build command line: quoted exe path + args.
  let mut cmd = OsString::new();
  cmd.push("\"");
  cmd.push(exe);
  cmd.push("\"");
  for arg in args {
    cmd.push(" ");
    cmd.push(arg);
  }
  let mut cmd_wide: Vec<u16> = cmd.encode_wide().collect();
  cmd_wide.push(0);

  let mut pi: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };
  let ok = unsafe {
    CreateProcessW(
      null(),
      cmd_wide.as_mut_ptr(),
      null(),
      null(),
      1,
      EXTENDED_STARTUPINFO_PRESENT,
      null(),
      null(),
      &startup.StartupInfo,
      &mut pi,
    )
  };

  // Always delete attribute list after CreateProcess returns.
  unsafe { DeleteProcThreadAttributeList(attr_list_ptr) };
  drop(attr_storage);

  if ok == 0 {
    return Err(format!(
      "CreateProcessW (AppContainer) failed: {}",
      std::io::Error::last_os_error()
    ));
  }

  Ok(pi)
}

fn wait_process(pi: &PROCESS_INFORMATION, timeout_ms: u32) -> Result<u32, String> {
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

#[test]
fn appcontainer_renderer_smoke() {
  let profile = match AppContainerProfile::create_unique() {
    Ok(profile) => profile,
    Err(AppContainerProfileError::NotSupported(reason)) => {
      eprintln!("skipping AppContainer renderer smoke test: {reason}");
      return;
    }
    Err(AppContainerProfileError::Failed(err)) => {
      panic!("failed to create AppContainer profile for smoke test: {err}");
    }
  };

  let exe = std::env::current_exe()
    .map_err(|e| format!("current_exe failed: {e}"))
    .unwrap();

  let exe_os = exe.as_os_str();
  let args = ["--ignored", "--exact", CHILD_TEST_NAME, "--nocapture"];
  let pi = spawn_appcontainer_process(&profile, exe_os, &args).expect("spawn AppContainer child");

  // Always close handles.
  let exit_code = wait_process(&pi, CHILD_TIMEOUT_MS).expect("wait for child process");
  unsafe {
    CloseHandle(pi.hThread);
    CloseHandle(pi.hProcess);
  }

  assert_eq!(
    exit_code, 0,
    "AppContainer child exited with non-zero code {exit_code}"
  );
}

fn format_error_chain(err: &(dyn std::error::Error)) -> String {
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
  // `GetCurrentProcess()` returns a pseudo-handle with value -1.
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
      &mut is_appcontainer as *mut _ as *mut std::ffi::c_void,
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

#[test]
#[ignore]
fn appcontainer_renderer_smoke_child() {
  if let Err(err) = ensure_running_in_appcontainer() {
    panic!("expected AppContainer sandbox, but child process is not in AppContainer: {err}");
  }
  if let Err(err) = appcontainer_renderer_smoke_child_inner() {
    let chain = format_error_chain(&err);
    panic!("AppContainer child failed to initialize FastRender and render minimal HTML:\n{chain}");
  }
}

fn appcontainer_renderer_smoke_child_inner() -> fastrender::Result<()> {
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
