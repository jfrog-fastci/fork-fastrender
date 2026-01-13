//! Windows sandbox security regression test: ensure the sandboxed child cannot obtain a process
//! handle to the parent (browser/broker) with high privileges.
//!
//! Threat model: even if filesystem/network are blocked, a compromised renderer could steal secrets
//! from the browser process by opening a handle with `PROCESS_VM_READ` / `PROCESS_DUP_HANDLE` and
//! reading memory or duplicating existing privileged handles.
//!
//! This test launches a child copy of the test harness inside an AppContainer and then attempts
//! `OpenProcess(PROCESS_VM_READ | PROCESS_QUERY_INFORMATION | PROCESS_DUP_HANDLE)` against the
//! parent PID. We expect `OpenProcess` to fail with `ERROR_ACCESS_DENIED`.

#![cfg(windows)]

use std::env;
use std::ffi::OsStr;
use std::os::windows::ffi::OsStrExt;
use std::path::PathBuf;

use windows_sys::Win32::Foundation::{
  CloseHandle, GetLastError, SetHandleInformation, ERROR_ACCESS_DENIED, HANDLE, HANDLE_FLAG_INHERIT,
  INVALID_HANDLE_VALUE,
};
use windows_sys::Win32::Security::Authorization::{
  CreateAppContainerProfile, DeriveAppContainerSidFromAppContainerName,
};
use windows_sys::Win32::Security::{FreeSid, SECURITY_CAPABILITIES};
use windows_sys::Win32::System::Console::{GetStdHandle, STD_ERROR_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE};
use windows_sys::Win32::System::Threading::{
  CreateProcessW, DeleteProcThreadAttributeList, GetExitCodeProcess, InitializeProcThreadAttributeList,
  OpenProcess, TerminateProcess, UpdateProcThreadAttribute, WaitForSingleObject, PROCESS_INFORMATION,
  PROCESS_DUP_HANDLE, PROCESS_QUERY_INFORMATION, PROCESS_VM_READ, PROC_THREAD_ATTRIBUTE_LIST,
  STARTUPINFOEXW,
};

const ENV_CHILD_MODE: &str = "FASTR_SANDBOX_TEST_CHILD";
const ENV_PARENT_PID: &str = "FASTR_SANDBOX_TEST_PARENT_PID";

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
    // Ensure the AppContainer profile exists (best-effort; older Windows builds may not support
    // AppContainer at all).
    let mut appcontainer_sid = std::ptr::null_mut();
    let appcontainer_name_w = wide_null_str(APP_CONTAINER_NAME);
    let display_name_w = wide_null_str("FastRender sandbox test");
    let description_w = wide_null_str("FastRender sandbox test AppContainer profile");

    let create_hr = CreateAppContainerProfile(
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
      let derive_hr = DeriveAppContainerSidFromAppContainerName(
        appcontainer_name_w.as_ptr(),
        &mut appcontainer_sid,
      );
      if derive_hr != 0 {
        // The API is not available on older Windows. Treat this as a skip to avoid false failures
        // on unsupported hosts.
        eprintln!(
          "skipping Windows AppContainer process-handle escape test: DeriveAppContainerSidFromAppContainerName failed (hr=0x{derive_hr:08X})"
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

fn run_child() {
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
    assert_eq!(
      err, ERROR_ACCESS_DENIED,
      "OpenProcess unexpectedly failed with error {err} (0x{err:08X}); expected ERROR_ACCESS_DENIED (5). Requested access mask: 0x{access:08X}",
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

  let exit = spawn_appcontainer_child(parent_pid, test_filter).expect("spawn sandboxed child");
  if exit != 0 {
    panic!("sandboxed child exited with code {exit}");
  }
}
