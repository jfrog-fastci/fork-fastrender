#![cfg(windows)]

use fastrender::sandbox::windows::spawn_sandboxed;
use std::ffi::OsString;
use std::mem;
use std::os::windows::io::{AsRawHandle, RawHandle};
use std::path::Path;
use std::ptr;
use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, HANDLE};
use windows_sys::Win32::Security::SECURITY_ATTRIBUTES;
use windows_sys::Win32::System::Threading::{
  CreateEventW, GetExitCodeProcess, WaitForSingleObject, WAIT_OBJECT_0, WAIT_TIMEOUT,
};

fn assert_child_exited_successfully(child: &fastrender::sandbox::windows::SandboxedChild) {
  unsafe {
    let proc_handle = child.process.as_raw_handle() as HANDLE;
    let exit_wait = WaitForSingleObject(proc_handle, 10_000);
    assert_eq!(
      exit_wait, WAIT_OBJECT_0,
      "child process did not exit cleanly; WaitForSingleObject returned {exit_wait}"
    );
    let mut exit_code: u32 = 0;
    assert!(
      GetExitCodeProcess(proc_handle, &mut exit_code) != 0,
      "GetExitCodeProcess failed: {}",
      GetLastError()
    );
    if exit_code != 0 {
      let hint = match exit_code {
        2 => "child argv parsing failed (expected --allowed <u64> --denied <u64>)",
        3 => "child SetEvent(allowed) failed (allowed handle not inherited/usable)",
        4 => "child SetEvent(denied) unexpectedly succeeded (denied handle leaked)",
        5 => "child SetEvent(denied) failed with unexpected error code",
        _ => "child exited with unexpected non-zero status",
      };
      panic!("child process exited with code {exit_code}: {hint}");
    }
  }
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

fn create_event(inheritable: bool) -> HandleGuard {
  unsafe {
    let mut attrs = SECURITY_ATTRIBUTES {
      nLength: mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
      lpSecurityDescriptor: ptr::null_mut(),
      bInheritHandle: if inheritable { 1 } else { 0 },
    };
    let handle = CreateEventW(
      if inheritable {
        &mut attrs as *mut SECURITY_ATTRIBUTES
      } else {
        ptr::null_mut()
      },
      1,
      0,
      ptr::null(),
    );
    assert!(handle != 0, "CreateEventW failed: {}", GetLastError());
    HandleGuard(handle)
  }
}

#[test]
fn sandbox_spawn_selective_handle_inheritance_proc_thread_attribute_handle_list() {
  // Assert that Windows sandboxed spawning uses `PROC_THREAD_ATTRIBUTE_HANDLE_LIST` to inherit only
  // explicitly allowlisted handles (capability leak prevention).

  // Inflate handle values a bit to reduce the chance of accidental collision with pre-existing
  // handles in the child process when we pass the raw numeric value for the denied handle.
  let _padding: Vec<HandleGuard> = (0..512).map(|_| create_event(false)).collect();

  let allowed = create_event(true);
  let denied = create_event(true);

  let child_exe = Path::new(env!("CARGO_BIN_EXE_sandbox_spawn_handle_inheritance_child"));
  let args: Vec<OsString> = vec![
    "--allowed".into(),
    (allowed.0 as u64).to_string().into(),
    "--denied".into(),
    (denied.0 as u64).to_string().into(),
  ];
  let inherit: [RawHandle; 1] = [allowed.0 as RawHandle];
  let child = spawn_sandboxed(child_exe, &args, &inherit).expect("spawn sandboxed child");

  unsafe {
    let wait_rc = WaitForSingleObject(allowed.0, 5_000);
    assert_eq!(
      wait_rc, WAIT_OBJECT_0,
      "expected allowed handle to be signaled by child; WaitForSingleObject returned {wait_rc}",
    );

    let denied_wait = WaitForSingleObject(denied.0, 0);
    assert_eq!(
      denied_wait, WAIT_TIMEOUT,
      "denied event should not be signaled by child"
    );
  }
  assert_child_exited_successfully(&child);

  // Keep the handles alive until after the child has exited (dropping closes them).
  let _ = (allowed, denied);
}

#[test]
fn sandbox_spawn_empty_inherit_handle_list_does_not_leak_inheritable_handles() {
  // When no handles are requested, the spawn helper must not enable blanket inheritance.
  // (bInheritHandles must be FALSE; otherwise all inheritable handles in the broker could leak.)

  let _padding: Vec<HandleGuard> = (0..512).map(|_| create_event(false)).collect();
  let denied = create_event(true);

  let child_exe = Path::new(env!("CARGO_BIN_EXE_sandbox_spawn_handle_inheritance_child"));
  let args: Vec<OsString> = vec![
    "--allowed".into(),
    "0".into(),
    "--denied".into(),
    (denied.0 as u64).to_string().into(),
  ];

  let child = spawn_sandboxed(child_exe, &args, &[]).expect("spawn sandboxed child");
  assert_child_exited_successfully(&child);

  unsafe {
    let denied_wait = WaitForSingleObject(denied.0, 0);
    assert_eq!(
      denied_wait, WAIT_TIMEOUT,
      "inheritable handle should not be inherited when inherit_handles is empty"
    );
  }

  let _ = denied;
}

