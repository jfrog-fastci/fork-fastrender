// Helper binary used by the Windows sandbox spawn handle inheritance integration test.
//
// The integration test needs a child process that does not rely on libtest/stdout/stderr, because
// `PROC_THREAD_ATTRIBUTE_HANDLE_LIST` can be used to intentionally restrict inherited handles.
// This binary performs minimal Win32 calls and communicates success/failure via exit code.

#[cfg(windows)]
fn main() {
  use windows_sys::Win32::Foundation::{
    GetLastError, ERROR_ACCESS_DENIED, ERROR_INVALID_HANDLE, HANDLE,
  };
  use windows_sys::Win32::System::Threading::SetEvent;

  let mut allowed: Option<u64> = None;
  let mut denied: Option<u64> = None;

  let mut args = std::env::args().skip(1);
  while let Some(arg) = args.next() {
    match arg.as_str() {
      "--allowed" => {
        let Some(value) = args.next() else {
          std::process::exit(2);
        };
        allowed = value.parse::<u64>().ok();
      }
      "--denied" => {
        let Some(value) = args.next() else {
          std::process::exit(2);
        };
        denied = value.parse::<u64>().ok();
      }
      _ => {}
    }
  }

  let Some(allowed_raw) = allowed else {
    std::process::exit(2);
  };
  let Some(denied_raw) = denied else {
    std::process::exit(2);
  };

  let allowed_handle: HANDLE = allowed_raw as isize;
  let denied_handle: HANDLE = denied_raw as isize;

  unsafe {
    // Allowed handle must be inherited and usable.
    if SetEvent(allowed_handle) == 0 {
      std::process::exit(3);
    }

    // Denied handle must NOT be inherited. It should fail with invalid handle / access denied.
    if SetEvent(denied_handle) != 0 {
      std::process::exit(4);
    }
    let err = GetLastError();
    if err != ERROR_INVALID_HANDLE && err != ERROR_ACCESS_DENIED {
      std::process::exit(5);
    }
  }
}

#[cfg(not(windows))]
fn main() {
  // This binary is only expected to be invoked by the Windows-only integration test.
  std::process::exit(0);
}
