#[cfg(windows)]
fn main() {
  use std::process;
  use windows_sys::Win32::Foundation::{GetLastError, ERROR_INVALID_HANDLE, HANDLE};
  use windows_sys::Win32::System::Threading::SetEvent;

  fn parse_handle_var(name: &str) -> HANDLE {
    let raw = std::env::var(name).unwrap_or_else(|_| panic!("missing env var {name}"));
    let value: u64 = raw
      .parse()
      .unwrap_or_else(|_| panic!("failed to parse {name}={raw:?} as u64"));
    value as usize as HANDLE
  }

  fn try_set(handle: HANDLE) -> Result<bool, u32> {
    unsafe {
      if SetEvent(handle) != 0 {
        return Ok(true);
      }
      let err = GetLastError();
      if err == ERROR_INVALID_HANDLE {
        return Ok(false);
      }
      Err(err)
    }
  }

  let handle_a = parse_handle_var("WIN_SANDBOX_TEST_HANDLE_A");
  let handle_b = parse_handle_var("WIN_SANDBOX_TEST_HANDLE_B");

  let mut exit_code: u32 = 0;

  match try_set(handle_a) {
    Ok(true) => exit_code |= 0b01,
    Ok(false) => {}
    Err(err) => {
      eprintln!("SetEvent(handle_a) failed with unexpected error {err}");
      process::exit(0x80);
    }
  }

  match try_set(handle_b) {
    Ok(true) => exit_code |= 0b10,
    Ok(false) => {}
    Err(err) => {
      eprintln!("SetEvent(handle_b) failed with unexpected error {err}");
      process::exit(0x81);
    }
  }

  process::exit(exit_code as i32);
}

#[cfg(not(windows))]
fn main() {
  eprintln!("handle_inheritance_child is Windows-only");
  std::process::exit(1);
}
