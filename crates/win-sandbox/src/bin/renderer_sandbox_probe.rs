#[cfg(not(windows))]
fn main() {}

#[cfg(windows)]
fn main() {
  if std::env::args_os().any(|a| a == "--grandchild") {
    // Grandchild payload: do nothing.
    std::process::exit(0);
  }

  match run_probe() {
    Ok(()) => std::process::exit(0),
    Err(code) => std::process::exit(code),
  }
}

#[cfg(windows)]
fn run_probe() -> Result<(), i32> {
  use windows_sys::Win32::Foundation::CloseHandle;
  use windows_sys::Win32::Security::{GetTokenInformation, TokenIsAppContainer, TOKEN_QUERY};
  use windows_sys::Win32::System::JobObjects::IsProcessInJob;
  use windows_sys::Win32::System::Threading::GetCurrentProcess;
  use win_sandbox::mitigations;

  #[link(name = "advapi32")]
  extern "system" {
    fn OpenProcessToken(
      process_handle: windows_sys::Win32::Foundation::HANDLE,
      desired_access: u32,
      token_handle: *mut windows_sys::Win32::Foundation::HANDLE,
    ) -> i32;
  }

  // 1) AppContainer check.
  let mut token: windows_sys::Win32::Foundation::HANDLE = core::ptr::null_mut();
  let ok = unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) };
  if ok == 0 {
    eprintln!(
      "OpenProcessToken failed: {}",
      std::io::Error::last_os_error()
    );
    return Err(10);
  }

  let mut is_appcontainer: u32 = 0;
  let mut ret_len: u32 = 0;
  let ok = unsafe {
    GetTokenInformation(
      token,
      TokenIsAppContainer,
      &mut is_appcontainer as *mut _ as *mut core::ffi::c_void,
      core::mem::size_of::<u32>() as u32,
      &mut ret_len,
    )
  };
  unsafe { CloseHandle(token) };
  if ok == 0 {
    eprintln!(
      "GetTokenInformation(TokenIsAppContainer) failed: {}",
      std::io::Error::last_os_error()
    );
    return Err(11);
  }
  if is_appcontainer == 0 {
    eprintln!("not running inside an AppContainer");
    return Err(12);
  }

  // 2) Job membership check.
  let mut in_job: i32 = 0;
  let ok = unsafe { IsProcessInJob(GetCurrentProcess(), core::ptr::null_mut(), &mut in_job) };
  if ok == 0 {
    eprintln!("IsProcessInJob failed: {}", std::io::Error::last_os_error());
    return Err(20);
  }
  if in_job == 0 {
    eprintln!("not running inside a Job object");
    return Err(21);
  }

  // 3) Mitigations check (best-effort; expected mask is computed based on OS support).
  if let Err(err) = mitigations::verify_renderer_mitigations_current_process() {
    eprintln!("renderer mitigations not active: {err}");
    return Err(22);
  }

  // 4) Grandchild spawn should fail (active process limit 1, no breakaway).
  let exe = std::env::current_exe().map_err(|_| 30)?;
  let mut cmd = std::process::Command::new(exe);
  cmd.arg("--grandchild");

  match cmd.spawn() {
    Ok(mut child) => {
      let _ = child.kill();
      let _ = child.wait();
      eprintln!("grandchild spawn unexpectedly succeeded");
      Err(31)
    }
    Err(err) => {
      // Typical failure is ERROR_ACCESS_DENIED (5).
      let _ = err;
      Ok(())
    }
  }
}
