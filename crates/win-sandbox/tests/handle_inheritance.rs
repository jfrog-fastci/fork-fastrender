#![cfg(windows)]

use std::ffi::OsString;
use std::path::PathBuf;
use win_sandbox::{spawn_sandboxed, RawHandle, SpawnConfig};
use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0, WAIT_TIMEOUT};
use windows_sys::Win32::Security::SECURITY_ATTRIBUTES;
use windows_sys::Win32::System::Threading::{CreateEventW, WaitForSingleObject};

unsafe fn create_inheritable_event() -> HANDLE {
    let mut sa: SECURITY_ATTRIBUTES = std::mem::zeroed();
    sa.nLength = std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32;
    sa.bInheritHandle = 1;
    sa.lpSecurityDescriptor = std::ptr::null_mut();

    // Manual-reset event, initial state = nonsignaled.
    let handle = CreateEventW(&sa, 1, 0, std::ptr::null());
    assert!(!handle.is_null(), "CreateEventW failed");
    handle
}

#[test]
fn handle_list_is_enforced() {
    let child_exe = PathBuf::from(env!("CARGO_BIN_EXE_handle_inheritance_child"));

    unsafe {
        let handle_a = create_inheritable_event();
        let handle_b = create_inheritable_event();

        let env = vec![
            (OsString::from("WIN_SANDBOX_TEST_HANDLE_A"), (handle_a as usize).to_string().into()),
            (OsString::from("WIN_SANDBOX_TEST_HANDLE_B"), (handle_b as usize).to_string().into()),
        ];

        // 1) No allow-list => bInheritHandles=FALSE => child cannot signal either handle.
        let cfg = SpawnConfig {
            exe: child_exe.clone(),
            args: Vec::new(),
            env: env.clone(),
            current_dir: None,
            inherit_handles: Vec::new(),
            appcontainer: None,
            job: None,
            mitigation_policy: None,
        };

        let child = spawn_sandboxed(&cfg).expect("spawn (no handles) failed");
        let code = child.wait().expect("wait (no handles) failed");
        assert_eq!(
            code, 0,
            "expected child to be unable to SetEvent on either handle, got exit code {code}"
        );

        assert_eq!(
            WaitForSingleObject(handle_a, 0),
            WAIT_TIMEOUT,
            "handle_a unexpectedly signaled"
        );
        assert_eq!(
            WaitForSingleObject(handle_b, 0),
            WAIT_TIMEOUT,
            "handle_b unexpectedly signaled"
        );

        // 2) Allow-list only handle_a => child should be able to signal A but not B.
        let cfg = SpawnConfig {
            exe: child_exe,
            args: Vec::new(),
            env,
            current_dir: None,
            inherit_handles: vec![handle_a as RawHandle],
            appcontainer: None,
            job: None,
            mitigation_policy: None,
        };

        let child = spawn_sandboxed(&cfg).expect("spawn (handle list) failed");
        let code = child.wait().expect("wait (handle list) failed");
        assert_eq!(
            code, 0b01,
            "expected child to signal only handle_a (bit0), got exit code {code}"
        );

        assert_eq!(
            WaitForSingleObject(handle_a, 0),
            WAIT_OBJECT_0,
            "handle_a expected signaled"
        );
        assert_eq!(
            WaitForSingleObject(handle_b, 0),
            WAIT_TIMEOUT,
            "handle_b unexpectedly signaled (HANDLE_LIST not enforced?)"
        );

        CloseHandle(handle_a);
        CloseHandle(handle_b);
    }
}
