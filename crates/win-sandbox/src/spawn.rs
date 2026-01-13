use crate::{AppContainerProfile, Job, OwnedHandle, RawHandle, WinSandboxError};

use std::ffi::{OsStr, OsString};
use std::os::windows::ffi::OsStrExt;
use std::path::PathBuf;
use std::ptr;
use std::time::Duration;

use windows_sys::Win32::Foundation::{
  ERROR_ACCESS_DENIED, ERROR_INSUFFICIENT_BUFFER, ERROR_INVALID_PARAMETER, ERROR_NOT_SUPPORTED,
  HANDLE, WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use windows_sys::Win32::Security::SECURITY_CAPABILITIES;
use windows_sys::Win32::System::JobObjects::IsProcessInJob;
use windows_sys::Win32::System::Threading::{
  CreateProcessW, DeleteProcThreadAttributeList, GetCurrentProcess, GetExitCodeProcess,
  InitializeProcThreadAttributeList, TerminateProcess, UpdateProcThreadAttribute,
  WaitForSingleObject, CREATE_BREAKAWAY_FROM_JOB, CREATE_UNICODE_ENVIRONMENT,
  EXTENDED_STARTUPINFO_PRESENT, INFINITE, LPPROC_THREAD_ATTRIBUTE_LIST, PROCESS_INFORMATION,
  PROC_THREAD_ATTRIBUTE_HANDLE_LIST, PROC_THREAD_ATTRIBUTE_JOB_LIST,
  PROC_THREAD_ATTRIBUTE_MITIGATION_POLICY, PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES,
  STARTUPINFOEXW, STARTUPINFOW,
};

// STARTUPINFOEX attribute value:
// ProcThreadAttributeValue(15, FALSE, TRUE, FALSE) → 0x0002_000F.
const PROC_THREAD_ATTRIBUTE_ALL_APPLICATION_PACKAGES_POLICY: usize = 0x0002_000F;

// Value for `PROC_THREAD_ATTRIBUTE_ALL_APPLICATION_PACKAGES_POLICY` (winbase.h).
const PROCESS_CREATION_ALL_APPLICATION_PACKAGES_POLICY_BLOCK: u32 = 1;

// `ERROR_NOT_SUPPORTED` / `ERROR_INVALID_PARAMETER` are stable Win32 error codes commonly returned
// when a `STARTUPINFOEX` attribute is not supported by the host OS.
const ERROR_NOT_SUPPORTED: u32 = 50;
const ERROR_INVALID_PARAMETER: u32 = 87;

/// Configuration for spawning a sandboxed Windows child process.
#[derive(Debug)]
pub struct SpawnConfig<'a> {
  pub exe: PathBuf,
  pub args: Vec<OsString>,
  pub env: Vec<(OsString, OsString)>,
  pub current_dir: Option<PathBuf>,
  pub inherit_handles: Vec<RawHandle>,
  pub appcontainer: Option<AppContainerProfile>,
  pub job: Option<&'a Job>,
  pub mitigation_policy: Option<u64>,
  /// When `true`, remove the broad `ALL APPLICATION PACKAGES` group (SID `S-1-15-2-1`) from the
  /// created AppContainer token via `PROC_THREAD_ATTRIBUTE_ALL_APPLICATION_PACKAGES_POLICY`.
  ///
  /// This is a defense-in-depth hardening layer: some system objects are ACL'd to
  /// `ALL APPLICATION PACKAGES`, so removing the group reduces ambient access for sandboxed
  /// processes.
  ///
  /// Compatibility note: older Windows builds may reject this attribute. The spawner treats it as
  /// optional and retries without it on `ERROR_INVALID_PARAMETER (87)` /
  /// `ERROR_NOT_SUPPORTED (50)`.
  pub all_application_packages_hardened: bool,
}

/// A spawned child process (Windows).
#[derive(Debug)]
pub struct ChildProcess {
  process: OwnedHandle,
  // Keep the primary thread handle alive; some callers may want to inspect it.
  _main_thread: OwnedHandle,
}

impl ChildProcess {
  pub(crate) fn new(process: HANDLE, main_thread: HANDLE) -> Self {
    Self {
      process: OwnedHandle::from_raw(process),
      _main_thread: OwnedHandle::from_raw(main_thread),
    }
  }

  /// Wait indefinitely for the process to exit and return its exit code.
  pub fn wait(&self) -> std::result::Result<u32, WinSandboxError> {
    let wait_rc = unsafe { WaitForSingleObject(self.process.as_raw(), INFINITE) };
    if wait_rc != WAIT_OBJECT_0 {
      return Err(WinSandboxError::last("WaitForSingleObject"));
    }

    let mut code: u32 = 0;
    let ok = unsafe { GetExitCodeProcess(self.process.as_raw(), &mut code) };
    if ok == 0 {
      return Err(WinSandboxError::last("GetExitCodeProcess"));
    }
    Ok(code)
  }

  /// Wait for the process to exit, up to `timeout`.
  ///
  /// Returns:
  /// - `Ok(Some(exit_code))` if the process exited
  /// - `Ok(None)` if the timeout elapsed
  pub fn wait_timeout(
    &self,
    timeout: Duration,
  ) -> std::result::Result<Option<u32>, WinSandboxError> {
    let timeout_ms = timeout.as_millis().min(u32::MAX as u128) as u32;

    let wait_rc = unsafe { WaitForSingleObject(self.process.as_raw(), timeout_ms) };
    if wait_rc == WAIT_OBJECT_0 {
      let mut code: u32 = 0;
      let ok = unsafe { GetExitCodeProcess(self.process.as_raw(), &mut code) };
      if ok == 0 {
        return Err(WinSandboxError::last("GetExitCodeProcess"));
      }
      return Ok(Some(code));
    }

    if wait_rc == WAIT_TIMEOUT {
      return Ok(None);
    }

    Err(WinSandboxError::last("WaitForSingleObject"))
  }

  pub fn kill(&self) -> std::result::Result<(), WinSandboxError> {
    let ok = unsafe { TerminateProcess(self.process.as_raw(), 1) };
    if ok == 0 {
      return Err(WinSandboxError::last("TerminateProcess"));
    }
    Ok(())
  }
}

pub(crate) fn wide_null(s: &OsStr) -> Vec<u16> {
  s.encode_wide().chain(Some(0)).collect()
}

pub(crate) fn build_command_line(exe: &OsStr, args: &[OsString]) -> Vec<u16> {
  let mut out: Vec<u16> = Vec::new();
  append_cmd_arg(&mut out, exe);
  for arg in args {
    out.push(b' ' as u16);
    append_cmd_arg(&mut out, arg.as_os_str());
  }
  out.push(0);
  out
}

fn append_cmd_arg(out: &mut Vec<u16>, arg: &OsStr) {
  let arg_w: Vec<u16> = arg.encode_wide().collect();
  let needs_quotes = arg_w.is_empty()
    || arg_w
      .iter()
      .any(|&c| c == b' ' as u16 || c == b'\t' as u16 || c == b'"' as u16);

  if !needs_quotes {
    out.extend_from_slice(&arg_w);
    return;
  }

  out.push(b'"' as u16);
  let mut backslashes: usize = 0;
  for &c in &arg_w {
    if c == b'\\' as u16 {
      backslashes += 1;
      continue;
    }

    if c == b'"' as u16 {
      for _ in 0..(backslashes * 2 + 1) {
        out.push(b'\\' as u16);
      }
      out.push(b'"' as u16);
      backslashes = 0;
      continue;
    }

    for _ in 0..backslashes {
      out.push(b'\\' as u16);
    }
    backslashes = 0;
    out.push(c);
  }

  for _ in 0..(backslashes * 2) {
    out.push(b'\\' as u16);
  }
  out.push(b'"' as u16);
}

pub(crate) fn build_environment_block(extra: &[(OsString, OsString)]) -> Option<Vec<u16>> {
  if extra.is_empty() {
    return None;
  }

  // Inherit the current environment, then apply/override any provided vars.
  let mut vars: Vec<(OsString, OsString)> = std::env::vars_os().collect();
  for (k, v) in extra {
    // Windows environment variables are case-insensitive, so treat overrides the same way.
    let key_norm = k.to_string_lossy().to_ascii_uppercase();
    vars.retain(|(ek, _)| ek.to_string_lossy().to_ascii_uppercase() != key_norm);
    vars.push((k.clone(), v.clone()));
  }

  // Sort by name (best-effort) as recommended by CreateProcessW docs.
  vars.sort_by_key(|(k, _)| k.to_string_lossy().to_ascii_uppercase());

  let mut block: Vec<u16> = Vec::new();
  for (k, v) in vars {
    block.extend(k.encode_wide());
    block.push(b'=' as u16);
    block.extend(v.encode_wide());
    block.push(0);
  }
  block.push(0);
  Some(block)
}

pub(crate) struct AttributeList {
  pub(crate) ptr: LPPROC_THREAD_ATTRIBUTE_LIST,
  _buf: Vec<usize>,
}

impl AttributeList {
  pub(crate) fn new(attribute_count: u32) -> std::result::Result<Self, WinSandboxError> {
    let mut size: usize = 0;
    let ok =
      unsafe { InitializeProcThreadAttributeList(ptr::null_mut(), attribute_count, 0, &mut size) };
    if ok != 0 {
      return Err(WinSandboxError::from_code(
        "InitializeProcThreadAttributeList(size query)",
        0,
      ));
    }

    let err = unsafe { windows_sys::Win32::Foundation::GetLastError() };
    if err != ERROR_INSUFFICIENT_BUFFER {
      return Err(WinSandboxError::from_code(
        "InitializeProcThreadAttributeList(size query)",
        err,
      ));
    }

    // Ensure the buffer is sufficiently aligned.
    let words = (size + std::mem::size_of::<usize>() - 1) / std::mem::size_of::<usize>();
    let mut buf: Vec<usize> = vec![0; words.max(1)];
    let ptr = buf.as_mut_ptr() as LPPROC_THREAD_ATTRIBUTE_LIST;

    let ok = unsafe { InitializeProcThreadAttributeList(ptr, attribute_count, 0, &mut size) };
    if ok == 0 {
      return Err(WinSandboxError::last("InitializeProcThreadAttributeList"));
    }

    Ok(Self { ptr, _buf: buf })
  }

  pub(crate) fn update(
    &mut self,
    attribute: usize,
    value_ptr: *mut core::ffi::c_void,
    value_size: usize,
  ) -> std::result::Result<(), WinSandboxError> {
    let ok = unsafe {
      UpdateProcThreadAttribute(
        self.ptr,
        0,
        attribute,
        value_ptr,
        value_size,
        ptr::null_mut(),
        ptr::null_mut(),
      )
    };
    if ok == 0 {
      return Err(WinSandboxError::last("UpdateProcThreadAttribute"));
    }
    Ok(())
  }
}

impl Drop for AttributeList {
  fn drop(&mut self) {
    unsafe {
      if !self.ptr.is_null() {
        DeleteProcThreadAttributeList(self.ptr);
      }
    }
  }
}

fn mitigation_policy_attribute_unsupported(err: &WinSandboxError) -> bool {
  match err {
    WinSandboxError::Win32 { code, .. } => {
      *code == ERROR_INVALID_PARAMETER || *code == ERROR_NOT_SUPPORTED
    }
    _ => false,
  }
}

/// Spawn a sandboxed process using `CreateProcessW` + `STARTUPINFOEXW` attributes.
///
/// ## Parent already in a Job object
///
/// When `cfg.job` is set and the current process is already running inside a Windows Job, the OS
/// may require the new child process to be created with `CREATE_BREAKAWAY_FROM_JOB` (depending on
/// nested-job support + parent job policy). In that case this helper:
///
/// 1. Tries `CreateProcessW` with `CREATE_BREAKAWAY_FROM_JOB`.
/// 2. Retries without breakaway if the breakaway attempt fails with `ERROR_ACCESS_DENIED`.
///
/// This is best-effort compatibility: it does not provide a "jobless" fallback mode.
pub fn spawn_sandboxed(
  cfg: &SpawnConfig<'_>,
) -> std::result::Result<ChildProcess, WinSandboxError> {
  // Optional mitigation policy application escape hatch.
  let mitigation_policy = match cfg.mitigation_policy {
    Some(bits) if bits != 0 && std::env::var_os("FASTR_DISABLE_WIN_MITIGATIONS").is_none() => {
      Some(bits)
    }
    _ => None,
  };

  match spawn_sandboxed_inner(cfg, mitigation_policy) {
    Ok(child) => Ok(child),
    Err(err) if mitigation_policy.is_some() && mitigation_policy_attribute_unsupported(&err) => {
      // Best-effort compatibility: if the OS doesn't recognize the mitigation policy attribute,
      // retry without it instead of failing process creation.
      spawn_sandboxed_inner(cfg, None)
    }
    Err(err) => Err(err),
  }
}

fn spawn_sandboxed_inner(
  cfg: &SpawnConfig<'_>,
  mitigation_policy: Option<u64>,
) -> std::result::Result<ChildProcess, WinSandboxError> {
  let exe_w = wide_null(cfg.exe.as_os_str());
  let mut cmdline = build_command_line(cfg.exe.as_os_str(), &cfg.args);

  let env_block = build_environment_block(&cfg.env);
  let env_ptr = env_block
    .as_ref()
    .map(|b| b.as_ptr() as *const core::ffi::c_void)
    .unwrap_or(ptr::null());

  let current_dir_w;
  let current_dir_ptr = match &cfg.current_dir {
    Some(dir) => {
      current_dir_w = wide_null(dir.as_os_str());
      current_dir_w.as_ptr()
    }
    None => ptr::null(),
  };

  let b_inherit_handles = if cfg.inherit_handles.is_empty() { 0 } else { 1 };

  let parent_in_job = {
    let mut in_job: i32 = 0;
    // SAFETY: `in_job` is a valid out param; null job handle queries "any job".
    let ok = unsafe { IsProcessInJob(GetCurrentProcess(), ptr::null_mut(), &mut in_job) };
    ok != 0 && in_job != 0
  };

  // Build any requested attributes.
  //
  // Note: `AppContainerProfile` can be disabled (no SID). Treat that as "no AppContainer".
  let needs_security_caps = cfg
    .appcontainer
    .as_ref()
    .and_then(|profile| profile.sid_opt())
    .is_some();
  let needs_job = cfg.job.is_some();
  let needs_handle_list = !cfg.inherit_handles.is_empty();
  let needs_mitigation = mitigation_policy.is_some();
  let needs_aap_policy = needs_security_caps && cfg.all_application_packages_hardened;

  let attribute_count = (needs_security_caps as u32)
    + (needs_job as u32)
    + (needs_handle_list as u32)
    + (needs_mitigation as u32)
    + (needs_aap_policy as u32);

  // If no attributes are required, avoid EXTENDED_STARTUPINFO_PRESENT; some Windows versions
  // reject it when no attribute list is present.
  if attribute_count == 0 {
    let mut si: STARTUPINFOW = unsafe { std::mem::zeroed() };
    si.cb = std::mem::size_of::<STARTUPINFOW>() as u32;

    let mut pi: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };
    let mut create_process_inner = |flags: u32| -> std::result::Result<(), WinSandboxError> {
      let ok = unsafe {
        CreateProcessW(
          exe_w.as_ptr(),
          cmdline.as_mut_ptr(),
          ptr::null(),
          ptr::null(),
          b_inherit_handles,
          flags,
          env_ptr as *const _ as *mut _,
          current_dir_ptr,
          &mut si,
          &mut pi,
        )
      };
      if ok == 0 {
        return Err(WinSandboxError::last("CreateProcessW"));
      }
      Ok(())
    };

    let flags = CREATE_UNICODE_ENVIRONMENT;
    if parent_in_job && cfg.job.is_some() {
      match create_process_inner(flags | CREATE_BREAKAWAY_FROM_JOB) {
        Ok(()) => {}
        Err(err) if matches!(err, WinSandboxError::Win32 { code, .. } if code == ERROR_ACCESS_DENIED) =>
        {
          create_process_inner(flags)?;
        }
        Err(err) => return Err(err),
      }
    } else {
      create_process_inner(flags)?;
    }

    return Ok(ChildProcess {
      process: OwnedHandle::from_raw(pi.hProcess),
      _main_thread: OwnedHandle::from_raw(pi.hThread),
    });
  }

  // Attribute values must live until after CreateProcessW returns.
  let mut inherit_handle_list: Vec<HANDLE> =
    cfg.inherit_handles.iter().map(|&h| h as HANDLE).collect();

  let mut job_handle: HANDLE = ptr::null_mut();
  if let Some(job) = cfg.job {
    job_handle = job.handle();
  }

  let mut mitigation_policy_value: u64 = mitigation_policy.unwrap_or(0);
  let mut all_packages_policy_value: u32 = PROCESS_CREATION_ALL_APPLICATION_PACKAGES_POLICY_BLOCK;

  let mut security_capabilities: SECURITY_CAPABILITIES = unsafe { std::mem::zeroed() };
  if let Some(profile) = &cfg.appcontainer {
    if let Some(sid) = profile.sid_opt() {
      security_capabilities.AppContainerSid = sid.as_ptr();
      security_capabilities.Capabilities = ptr::null_mut();
      security_capabilities.CapabilityCount = 0;
      security_capabilities.Reserved = 0;
    }
  }

  let is_optional_attr_error = |err: &WinSandboxError| {
    matches!(
      err,
      WinSandboxError::Win32 { code, .. }
        if *code == ERROR_NOT_SUPPORTED || *code == ERROR_INVALID_PARAMETER
    )
  };

  let mut attr_list = AttributeList::new(attribute_count)?;
  if needs_security_caps {
    attr_list.update(
      PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES as usize,
      (&mut security_capabilities as *mut SECURITY_CAPABILITIES).cast(),
      std::mem::size_of::<SECURITY_CAPABILITIES>(),
    )?;
  }
  let mut aap_policy_applied = false;
  if needs_aap_policy {
    match attr_list.update(
      PROC_THREAD_ATTRIBUTE_ALL_APPLICATION_PACKAGES_POLICY,
      (&mut all_packages_policy_value as *mut u32).cast(),
      std::mem::size_of::<u32>(),
    ) {
      Ok(()) => {
        aap_policy_applied = true;
      }
      Err(err) if is_optional_attr_error(&err) => {
        // Best-effort: older Windows builds may reject this attribute.
      }
      Err(err) => return Err(err),
    }
  }
  if needs_job {
    attr_list.update(
      PROC_THREAD_ATTRIBUTE_JOB_LIST as usize,
      (&mut job_handle as *mut HANDLE).cast(),
      std::mem::size_of::<HANDLE>(),
    )?;
  }
  if needs_handle_list {
    attr_list.update(
      PROC_THREAD_ATTRIBUTE_HANDLE_LIST as usize,
      inherit_handle_list.as_mut_ptr().cast(),
      inherit_handle_list.len() * std::mem::size_of::<HANDLE>(),
    )?;
  }
  if needs_mitigation {
    attr_list.update(
      PROC_THREAD_ATTRIBUTE_MITIGATION_POLICY as usize,
      (&mut mitigation_policy_value as *mut u64).cast(),
      std::mem::size_of::<u64>(),
    )?;
  }

  let mut si_ex: STARTUPINFOEXW = unsafe { std::mem::zeroed() };
  si_ex.StartupInfo.cb = std::mem::size_of::<STARTUPINFOEXW>() as u32;
  si_ex.lpAttributeList = attr_list.ptr;

  let mut pi: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };
  let flags = CREATE_UNICODE_ENVIRONMENT | EXTENDED_STARTUPINFO_PRESENT;

  let mut create_process_inner = |flags: u32| -> std::result::Result<(), WinSandboxError> {
    // CreateProcessW may mutate the command line buffer, even on failure.
    cmdline = build_command_line(cfg.exe.as_os_str(), &cfg.args);
    let ok = unsafe {
      CreateProcessW(
        exe_w.as_ptr(),
        cmdline.as_mut_ptr(),
        ptr::null(),
        ptr::null(),
        b_inherit_handles,
        flags,
        env_ptr as *const _ as *mut _,
        current_dir_ptr,
        &mut si_ex.StartupInfo,
        &mut pi,
      )
    };
    if ok == 0 {
      return Err(WinSandboxError::last("CreateProcessW"));
    }
    Ok(())
  };

  let create_process_with_optional_breakaway = |create: &mut dyn FnMut(
    u32,
  )
    -> std::result::Result<
    (),
    WinSandboxError,
  >| {
    if parent_in_job && needs_job {
      match create(flags | CREATE_BREAKAWAY_FROM_JOB) {
        Ok(()) => Ok(()),
        Err(err) if matches!(err, WinSandboxError::Win32 { code, .. } if code == ERROR_ACCESS_DENIED) => {
          create(flags)
        }
        Err(err) => Err(err),
      }
    } else {
      create(flags)
    }
  };

  match create_process_with_optional_breakaway(&mut create_process_inner) {
    Ok(()) => {}
    Err(err) => {
      if aap_policy_applied && is_optional_attr_error(&err) {
        // Some Windows builds reject `PROC_THREAD_ATTRIBUTE_ALL_APPLICATION_PACKAGES_POLICY` at
        // CreateProcess time even if `UpdateProcThreadAttribute` accepted it. Retry without it.
        let attribute_count_without_aap = attribute_count.saturating_sub(1);
        let mut attr_list = AttributeList::new(attribute_count_without_aap)?;
        if needs_security_caps {
          attr_list.update(
            PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES as usize,
            (&mut security_capabilities as *mut SECURITY_CAPABILITIES).cast(),
            std::mem::size_of::<SECURITY_CAPABILITIES>(),
          )?;
        }
        if needs_job {
          attr_list.update(
            PROC_THREAD_ATTRIBUTE_JOB_LIST as usize,
            (&mut job_handle as *mut HANDLE).cast(),
            std::mem::size_of::<HANDLE>(),
          )?;
        }
        if needs_handle_list {
          attr_list.update(
            PROC_THREAD_ATTRIBUTE_HANDLE_LIST as usize,
            inherit_handle_list.as_mut_ptr().cast(),
            inherit_handle_list.len() * std::mem::size_of::<HANDLE>(),
          )?;
        }
        if needs_mitigation {
          attr_list.update(
            PROC_THREAD_ATTRIBUTE_MITIGATION_POLICY as usize,
            (&mut mitigation_policy_value as *mut u64).cast(),
            std::mem::size_of::<u64>(),
          )?;
        }

        let mut si_ex: STARTUPINFOEXW = unsafe { std::mem::zeroed() };
        si_ex.StartupInfo.cb = std::mem::size_of::<STARTUPINFOEXW>() as u32;
        si_ex.lpAttributeList = attr_list.ptr;

        let mut pi: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };
        let mut create_process_inner = |flags: u32| -> std::result::Result<(), WinSandboxError> {
          cmdline = build_command_line(cfg.exe.as_os_str(), &cfg.args);
          let ok = unsafe {
            CreateProcessW(
              exe_w.as_ptr(),
              cmdline.as_mut_ptr(),
              ptr::null(),
              ptr::null(),
              b_inherit_handles,
              flags,
              env_ptr as *const _ as *mut _,
              current_dir_ptr,
              &mut si_ex.StartupInfo,
              &mut pi,
            )
          };
          if ok == 0 {
            return Err(WinSandboxError::last("CreateProcessW"));
          }
          Ok(())
        };

        create_process_with_optional_breakaway(&mut create_process_inner)?;

        return Ok(ChildProcess {
          process: OwnedHandle::from_raw(pi.hProcess),
          _main_thread: OwnedHandle::from_raw(pi.hThread),
        });
      }

      return Err(err);
    }
  }

  Ok(ChildProcess {
    process: OwnedHandle::from_raw(pi.hProcess),
    _main_thread: OwnedHandle::from_raw(pi.hThread),
  })
}
