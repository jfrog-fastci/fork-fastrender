use crate::{OwnedHandle, Result, WinSandboxError};

/// A spawned child process (process + primary thread handles).
#[derive(Debug)]
pub struct ChildProcess {
  process: OwnedHandle,
  #[allow(dead_code)]
  thread: OwnedHandle,
  pid: u32,
}

impl ChildProcess {
  pub(crate) fn new(
    process: windows_sys::Win32::Foundation::HANDLE,
    thread: windows_sys::Win32::Foundation::HANDLE,
    pid: u32,
  ) -> Self {
    Self {
      process: OwnedHandle::from_raw(process),
      thread: OwnedHandle::from_raw(thread),
      pid,
    }
  }

  pub fn id(&self) -> u32 {
    self.pid
  }

  /// Blocks until the process exits and returns its exit code.
  pub fn wait(&mut self) -> Result<u32> {
    use windows_sys::Win32::Foundation::WAIT_FAILED;
    use windows_sys::Win32::System::Threading::{GetExitCodeProcess, WaitForSingleObject, INFINITE};

    let wait = unsafe { WaitForSingleObject(self.process.as_raw(), INFINITE) };
    if wait == WAIT_FAILED {
      return Err(WinSandboxError::last("WaitForSingleObject"));
    }

    let mut code: u32 = 0;
    let ok = unsafe { GetExitCodeProcess(self.process.as_raw(), &mut code) };
    if ok == 0 {
      return Err(WinSandboxError::last("GetExitCodeProcess"));
    }
    Ok(code)
  }

  pub fn terminate(&self, exit_code: u32) -> Result<()> {
    let ok = unsafe {
      windows_sys::Win32::System::Threading::TerminateProcess(self.process.as_raw(), exit_code)
    };
    if ok == 0 {
      return Err(WinSandboxError::last("TerminateProcess"));
    }
    Ok(())
  }
}
