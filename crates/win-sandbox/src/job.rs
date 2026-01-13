use crate::{OwnedHandle, Result, WinSandboxError};

use std::ffi::{c_void, OsStr};
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::AsRawHandle;

use windows_sys::Win32::System::JobObjects::{
  AssignProcessToJobObject, CreateJobObjectW, JobObjectBasicUIRestrictions,
  JobObjectExtendedLimitInformation, QueryInformationJobObject, SetInformationJobObject,
  TerminateJobObject, JOBOBJECT_BASIC_UI_RESTRICTIONS, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
  JOB_OBJECT_LIMIT_ACTIVE_PROCESS, JOB_OBJECT_LIMIT_BREAKAWAY_OK, JOB_OBJECT_LIMIT_JOB_MEMORY,
  JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE, JOB_OBJECT_LIMIT_SILENT_BREAKAWAY_OK,
  JOB_OBJECT_UILIMIT_DESKTOP, JOB_OBJECT_UILIMIT_DISPLAYSETTINGS, JOB_OBJECT_UILIMIT_EXITWINDOWS,
  JOB_OBJECT_UILIMIT_GLOBALATOMS, JOB_OBJECT_UILIMIT_HANDLES, JOB_OBJECT_UILIMIT_READCLIPBOARD,
  JOB_OBJECT_UILIMIT_SYSTEMPARAMETERS, JOB_OBJECT_UILIMIT_WRITECLIPBOARD,
};

/// A Windows Job object.
///
/// # Important edge cases
///
/// ## Parent already running inside a job
///
/// Some environments (CI systems, sandboxed shells, game launchers, etc.) run
/// the parent process inside a Job object already. On Windows 8+ *nested jobs*
/// are supported, so assigning a child process to a new job will generally work
/// (the child becomes associated with a chain of jobs).
///
/// On older Windows versions, a process can only be associated with one job. In
/// that case, attempting to place a child in this job may fail.
///
/// ## Breakaway
///
/// Windows supports *breakaway* semantics where a process can escape its job at
/// creation time if the job is configured to allow it and the creator uses the
/// `CREATE_BREAKAWAY_FROM_JOB` flag.
///
/// This wrapper intentionally **clears** `JOB_OBJECT_LIMIT_BREAKAWAY_OK` and
/// `JOB_OBJECT_LIMIT_SILENT_BREAKAWAY_OK` whenever it updates extended job
/// limits so callers don't accidentally create a job that allows sandboxed
/// renderer processes to break away.
#[derive(Debug)]
pub struct Job {
  handle: OwnedHandle,
}

impl Job {
  /// Creates (or opens) a named Windows Job object.
  ///
  /// If `name` is `None`, an unnamed job is created.
  pub fn new(name: Option<&str>) -> Result<Self> {
    if let Some(name) = name {
      if name.chars().any(|c| c == '\0') {
        return Err(WinSandboxError::InteriorNul { arg: "job_name" });
      }
    }

    let name_wide;
    let name_ptr = if let Some(name) = name {
      name_wide = OsStr::new(name)
        .encode_wide()
        .chain(Some(0))
        .collect::<Vec<u16>>();
      name_wide.as_ptr()
    } else {
      std::ptr::null()
    };

    // SAFETY: `CreateJobObjectW` expects a pointer to a null-terminated UTF-16
    // string or null; we ensure it stays alive for the call.
    let handle = unsafe { CreateJobObjectW(std::ptr::null(), name_ptr) };
    if handle.is_null() {
      return Err(WinSandboxError::last("CreateJobObjectW"));
    }

    Ok(Self {
      handle: OwnedHandle::from_raw(handle),
    })
  }

  /// Returns the raw Windows `HANDLE` for this job.
  pub fn handle(&self) -> windows_sys::Win32::Foundation::HANDLE {
    self.handle.as_raw()
  }

  /// Assigns a process to this job.
  ///
  /// This is the call that actually "sandboxes" the process: job limits apply
  /// to processes associated with the job.
  pub fn assign_process(&self, process: &impl AsRawHandle) -> Result<()> {
    let process = process.as_raw_handle() as windows_sys::Win32::Foundation::HANDLE;

    // SAFETY: `AssignProcessToJobObject` expects a valid job handle and a valid
    // process handle.
    let ok = unsafe { AssignProcessToJobObject(self.handle.as_raw(), process) };
    if ok == 0 {
      return Err(WinSandboxError::last("AssignProcessToJobObject"));
    }
    Ok(())
  }

  /// Terminates all processes currently associated with this job.
  ///
  /// This is typically not needed when `KILL_ON_JOB_CLOSE` is enabled (dropping the job handle will
  /// kill the process tree), but it can be useful for early shutdown or test cleanup.
  pub fn terminate(&self, exit_code: u32) -> Result<()> {
    // SAFETY: The job handle is valid for the duration of the call.
    let ok = unsafe { TerminateJobObject(self.handle.as_raw(), exit_code) };
    if ok == 0 {
      return Err(WinSandboxError::last("TerminateJobObject"));
    }
    Ok(())
  }

  /// Applies the baseline renderer restrictions to this job.
  ///
  /// This is a convenience wrapper around the individual `set_*` methods and
  /// is intended for the common "sandbox a renderer process" case:
  ///
  /// - Kill all processes in the job when the last job handle closes.
  /// - Prevent child process creation (`ActiveProcessLimit = 1`).
  /// - Optionally cap total committed memory for the job.
  /// - Apply headless UI restrictions.
  pub fn set_renderer_limits(&self, job_memory_limit_bytes: Option<usize>) -> Result<()> {
    self.set_kill_on_close()?;
    self.set_active_process_limit(1)?;
    if let Some(bytes) = job_memory_limit_bytes {
      self.set_job_memory_limit_bytes(bytes)?;
    }
    self.set_ui_restrictions_headless()?;
    Ok(())
  }

  /// Enables `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`.
  pub fn set_kill_on_close(&self) -> Result<()> {
    self.update_extended_limits(|info| {
      info.BasicLimitInformation.LimitFlags |= JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
    })
  }

  /// Sets `JOB_OBJECT_LIMIT_ACTIVE_PROCESS`.
  ///
  /// A limit of `1` prevents the sandboxed process from creating any children
  /// (because the job already contains the renderer itself).
  pub fn set_active_process_limit(&self, limit: u32) -> Result<()> {
    self.update_extended_limits(|info| {
      info.BasicLimitInformation.LimitFlags |= JOB_OBJECT_LIMIT_ACTIVE_PROCESS;
      info.BasicLimitInformation.ActiveProcessLimit = limit;
    })
  }

  /// Sets a job-wide committed memory limit in bytes.
  ///
  /// This uses `JOB_OBJECT_LIMIT_JOB_MEMORY` and sets
  /// `JOBOBJECT_EXTENDED_LIMIT_INFORMATION::JobMemoryLimit`.
  ///
  /// Semantics: the limit applies to the *total committed memory* across all
  /// processes in the job (not per-process). Allocations that would exceed
  /// the limit fail.
  pub fn set_job_memory_limit_bytes(&self, bytes: usize) -> Result<()> {
    self.update_extended_limits(|info| {
      info.BasicLimitInformation.LimitFlags |= JOB_OBJECT_LIMIT_JOB_MEMORY;
      info.JobMemoryLimit = bytes;
    })
  }

  /// Applies conservative UI restrictions suitable for a headless renderer.
  pub fn set_ui_restrictions_headless(&self) -> Result<()> {
    let mut restrictions = JOBOBJECT_BASIC_UI_RESTRICTIONS {
      UIRestrictionsClass: JOB_OBJECT_UILIMIT_HANDLES
        | JOB_OBJECT_UILIMIT_READCLIPBOARD
        | JOB_OBJECT_UILIMIT_WRITECLIPBOARD
        | JOB_OBJECT_UILIMIT_SYSTEMPARAMETERS
        | JOB_OBJECT_UILIMIT_DISPLAYSETTINGS
        | JOB_OBJECT_UILIMIT_GLOBALATOMS
        | JOB_OBJECT_UILIMIT_DESKTOP
        | JOB_OBJECT_UILIMIT_EXITWINDOWS,
    };

    // SAFETY: We provide a valid pointer to the expected structure.
    let ok = unsafe {
      SetInformationJobObject(
        self.handle.as_raw(),
        JobObjectBasicUIRestrictions,
        &mut restrictions as *mut _ as *mut c_void,
        std::mem::size_of::<JOBOBJECT_BASIC_UI_RESTRICTIONS>() as u32,
      )
    };
    if ok == 0 {
      return Err(WinSandboxError::last(
        "SetInformationJobObject(JobObjectBasicUIRestrictions)",
      ));
    }
    Ok(())
  }

  fn update_extended_limits<F>(&self, f: F) -> Result<()>
  where
    F: FnOnce(&mut JOBOBJECT_EXTENDED_LIMIT_INFORMATION),
  {
    let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
    let mut returned: u32 = 0;

    // SAFETY: `info` is a valid output buffer.
    let ok = unsafe {
      QueryInformationJobObject(
        self.handle.as_raw(),
        JobObjectExtendedLimitInformation,
        &mut info as *mut _ as *mut c_void,
        std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        &mut returned,
      )
    };
    if ok == 0 {
      return Err(WinSandboxError::last(
        "QueryInformationJobObject(JobObjectExtendedLimitInformation)",
      ));
    }

    f(&mut info);

    // Enforce "no breakaway" invariants.
    info.BasicLimitInformation.LimitFlags &=
      !(JOB_OBJECT_LIMIT_BREAKAWAY_OK | JOB_OBJECT_LIMIT_SILENT_BREAKAWAY_OK);

    // SAFETY: We provide a valid pointer to the expected structure.
    let ok = unsafe {
      SetInformationJobObject(
        self.handle.as_raw(),
        JobObjectExtendedLimitInformation,
        &mut info as *mut _ as *mut c_void,
        std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
      )
    };
    if ok == 0 {
      return Err(WinSandboxError::last(
        "SetInformationJobObject(JobObjectExtendedLimitInformation)",
      ));
    }
    Ok(())
  }
}

#[cfg(test)]
mod tests {
  use super::Job;

  #[test]
  fn job_api_smoke() {
    // Exercise the individual APIs on a fresh job.
    let job = Job::new(None).expect("create job");
    job.set_kill_on_close().expect("kill on close");
    job
      .set_active_process_limit(1)
      .expect("active process limit");
    job
      .set_job_memory_limit_bytes(64 * 1024 * 1024)
      .expect("job memory limit");
    job.set_ui_restrictions_headless().expect("ui restrictions");
    job.terminate(0).expect("terminate job");

    // Exercise the convenience helper on its own job to avoid relying on whether UI restrictions
    // can be set multiple times for the same job object.
    let job = Job::new(None).expect("create second job");
    job
      .set_renderer_limits(Some(64 * 1024 * 1024))
      .expect("renderer limits");
    job.terminate(0).expect("terminate second job");
  }
}
