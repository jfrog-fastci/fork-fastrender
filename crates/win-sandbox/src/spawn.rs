use std::{ffi::OsStr, os::windows::ffi::OsStrExt, path::Path, time::Duration};

use crate::{LastError, OwnedHandle, Result, WinSandboxError};

use windows_sys::Win32::{
  Foundation::CloseHandle,
  System::Threading::{
    CreateProcessW, DeleteProcThreadAttributeList, GetExitCodeProcess,
    InitializeProcThreadAttributeList, UpdateProcThreadAttribute, WaitForSingleObject,
    EXTENDED_STARTUPINFO_PRESENT, PROCESS_INFORMATION, STARTUPINFOEXW,
  },
};

/// A spawned sandboxed process (Windows).
///
/// This is a minimal wrapper around a process handle, intended for tests and sandbox orchestration.
pub struct SandboxedChild {
  process: OwnedHandle,
}

impl SandboxedChild {
  /// Wait for the process to exit.
  ///
  /// Returns:
  /// - `Ok(None)` if the timeout elapsed
  /// - `Ok(Some(exit_code))` if the process exited
  pub fn wait(&mut self, timeout: Duration) -> Result<Option<u32>> {
    let timeout_ms = timeout.as_millis().min(u32::MAX as u128) as u32;

    // SAFETY: `self.process` is a valid process HANDLE while `SandboxedChild` is alive.
    let wait_result = unsafe { WaitForSingleObject(self.process.as_raw(), timeout_ms) };
    if wait_result == windows_sys::Win32::Foundation::WAIT_OBJECT_0 {
      let mut exit_code: u32 = 0;
      // SAFETY: `self.process` is valid.
      let ok = unsafe { GetExitCodeProcess(self.process.as_raw(), &mut exit_code) };
      if ok == 0 {
        return Err(WinSandboxError::last("GetExitCodeProcess"));
      }
      return Ok(Some(exit_code));
    }

    if wait_result == windows_sys::Win32::Foundation::WAIT_TIMEOUT {
      return Ok(None);
    }

    Err(WinSandboxError::last("WaitForSingleObject"))
  }
}

fn encode_wide_nul(s: &OsStr) -> Vec<u16> {
  let mut wide: Vec<u16> = s.encode_wide().collect();
  wide.push(0);
  wide
}

fn append_arg_escaped(cmd: &mut Vec<u16>, arg: &OsStr) {
  let arg_wide: Vec<u16> = arg.encode_wide().collect();
  let needs_quotes = arg_wide.is_empty()
    || arg_wide
      .iter()
      .any(|&c| c == b' ' as u16 || c == b'\t' as u16 || c == b'"' as u16);

  if !needs_quotes {
    cmd.extend_from_slice(&arg_wide);
    return;
  }

  cmd.push(b'"' as u16);
  let mut backslashes = 0usize;
  for &ch in &arg_wide {
    if ch == b'\\' as u16 {
      backslashes += 1;
      continue;
    }

    if ch == b'"' as u16 {
      // Escape all backslashes + the quote.
      for _ in 0..(backslashes * 2 + 1) {
        cmd.push(b'\\' as u16);
      }
      cmd.push(b'"' as u16);
      backslashes = 0;
      continue;
    }

    // Emit accumulated backslashes as-is.
    for _ in 0..backslashes {
      cmd.push(b'\\' as u16);
    }
    backslashes = 0;
    cmd.push(ch);
  }

  // Escape trailing backslashes (they would otherwise escape the closing quote).
  for _ in 0..(backslashes * 2) {
    cmd.push(b'\\' as u16);
  }
  cmd.push(b'"' as u16);
}

fn build_command_line<A: AsRef<OsStr>>(program: &Path, args: &[A]) -> Vec<u16> {
  let mut cmd: Vec<u16> = Vec::new();
  append_arg_escaped(&mut cmd, program.as_os_str());
  for arg in args {
    cmd.push(b' ' as u16);
    append_arg_escaped(&mut cmd, arg.as_ref());
  }
  cmd.push(0);
  cmd
}

/// Spawns a sandboxed process with the given Windows process mitigation policy attribute.
///
/// This is a best-effort defense-in-depth layer intended to be used *in addition* to stronger
/// sandbox primitives like AppContainer and job objects.
///
/// Escape hatch:
/// - Set `FASTR_DISABLE_WIN_MITIGATIONS=1` to disable **mitigation policies only**.
///   This is intended for debugging or for compatibility with unusual Windows configurations.
pub fn spawn_sandboxed<A: AsRef<OsStr>>(
  program: &Path,
  args: &[A],
  mitigation_policy: u64,
) -> Result<SandboxedChild> {
  let mitigation_policy = if std::env::var_os("FASTR_DISABLE_WIN_MITIGATIONS").is_some() {
    0
  } else {
    mitigation_policy
  };

  let application_name = encode_wide_nul(program.as_os_str());
  let mut command_line = build_command_line(program, args);

  // SAFETY: Win32 API usage; all pointers are valid for the duration of the call.
  unsafe {
    let mut proc_info: PROCESS_INFORMATION = std::mem::zeroed();

    if mitigation_policy == 0 {
      // Use the standard STARTUPINFOEX path (without extended attributes) to keep the code
      // consistent; passing EXTENDED_STARTUPINFO_PRESENT with a null attribute list can fail on
      // some Windows versions.
      let mut startup: windows_sys::Win32::System::Threading::STARTUPINFOW = std::mem::zeroed();
      startup.cb = std::mem::size_of_val(&startup) as u32;

      let ok = CreateProcessW(
        application_name.as_ptr(),
        command_line.as_mut_ptr(),
        std::ptr::null(),
        std::ptr::null(),
        0,
        0,
        std::ptr::null(),
        std::ptr::null(),
        &mut startup,
        &mut proc_info,
      );

      if ok == 0 {
        return Err(WinSandboxError::last("CreateProcessW"));
      }

      // We don't need the primary thread handle after creation.
      CloseHandle(proc_info.hThread);

      return Ok(SandboxedChild {
        process: OwnedHandle::from_raw(proc_info.hProcess),
      });
    }

    // Create an attribute list with a single attribute: PROC_THREAD_ATTRIBUTE_MITIGATION_POLICY.
    let mut attr_list_size: usize = 0;
    let ok = InitializeProcThreadAttributeList(std::ptr::null_mut(), 1, 0, &mut attr_list_size);
    if ok != 0 {
      return Err(WinSandboxError::from_code(
        "InitializeProcThreadAttributeList(size query)",
        0,
      ));
    }
    let init_err = LastError::last().code();
    if init_err != windows_sys::Win32::Foundation::ERROR_INSUFFICIENT_BUFFER {
      return Err(WinSandboxError::from_code(
        "InitializeProcThreadAttributeList(size query)",
        init_err,
      ));
    }

    let mut attr_list_buf: Vec<u64> = vec![0; (attr_list_size + 7) / 8];
    let attr_list_ptr = attr_list_buf.as_mut_ptr()
      as windows_sys::Win32::System::Threading::LPPROC_THREAD_ATTRIBUTE_LIST;

    let ok = InitializeProcThreadAttributeList(attr_list_ptr, 1, 0, &mut attr_list_size);
    if ok == 0 {
      return Err(WinSandboxError::last("InitializeProcThreadAttributeList"));
    }

    let mut mitigation_policy_value = mitigation_policy;

    // The attribute value matches `ProcThreadAttributeValue(7, FALSE, TRUE, FALSE)` → 0x20007.
    const PROC_THREAD_ATTRIBUTE_MITIGATION_POLICY: usize = 0x0002_0007;
    let ok = UpdateProcThreadAttribute(
      attr_list_ptr,
      0,
      PROC_THREAD_ATTRIBUTE_MITIGATION_POLICY,
      &mut mitigation_policy_value as *mut u64 as *mut _,
      std::mem::size_of::<u64>(),
      std::ptr::null_mut(),
      std::ptr::null_mut(),
    );
    if ok == 0 {
      DeleteProcThreadAttributeList(attr_list_ptr);
      return Err(WinSandboxError::last("UpdateProcThreadAttribute"));
    }

    let mut startup_ex: STARTUPINFOEXW = std::mem::zeroed();
    startup_ex.StartupInfo.cb = std::mem::size_of::<STARTUPINFOEXW>() as u32;
    startup_ex.lpAttributeList = attr_list_ptr;

    let ok = CreateProcessW(
      application_name.as_ptr(),
      command_line.as_mut_ptr(),
      std::ptr::null(),
      std::ptr::null(),
      0,
      EXTENDED_STARTUPINFO_PRESENT,
      std::ptr::null(),
      std::ptr::null(),
      &mut startup_ex.StartupInfo,
      &mut proc_info,
    );

    DeleteProcThreadAttributeList(attr_list_ptr);

    if ok == 0 {
      return Err(WinSandboxError::last("CreateProcessW"));
    }

    CloseHandle(proc_info.hThread);

    Ok(SandboxedChild {
      process: OwnedHandle::from_raw(proc_info.hProcess),
    })
  }
}
