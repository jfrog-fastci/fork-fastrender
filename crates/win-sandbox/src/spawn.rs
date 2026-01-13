//! Process spawning helpers for the Windows sandbox layer.
//!
//! This module provides [`spawn_sandboxed`] plus shared plumbing for:
//! - Windows Job objects (lifetime + process-count limits)
//! - AppContainer process creation (via `PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES`)
//! - Restricted-token fallback (delegated to [`crate::restricted_token`])
//! - `STARTUPINFOEX` attribute list management
//! - Windows command line quoting

#![cfg(windows)]

use std::ffi::{c_void, OsStr, OsString};
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use windows_sys::Win32::Foundation::{
  ERROR_CALL_NOT_IMPLEMENTED, ERROR_INSUFFICIENT_BUFFER, ERROR_MOD_NOT_FOUND, ERROR_NOT_SUPPORTED,
  ERROR_PROC_NOT_FOUND, FALSE, HANDLE, TRUE, WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use windows_sys::Win32::Security::SECURITY_CAPABILITIES;
use windows_sys::Win32::System::Threading::{
  CreateProcessW, DeleteProcThreadAttributeList, GetExitCodeProcess, InitializeProcThreadAttributeList,
  ResumeThread, TerminateProcess, UpdateProcThreadAttribute, WaitForSingleObject, CREATE_SUSPENDED,
  EXTENDED_STARTUPINFO_PRESENT, LPPROC_THREAD_ATTRIBUTE_LIST, PROCESS_INFORMATION,
  PROC_THREAD_ATTRIBUTE_HANDLE_LIST, PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES, STARTUPINFOEXW,
  STARTUPINFOW,
};

use crate::{AppContainerProfile, Job, LastError, OwnedHandle, Result, WinSandboxError};

/// Attribute key for `PROC_THREAD_ATTRIBUTE_MITIGATION_POLICY`.
///
/// windows-sys does not currently expose this constant.
pub(crate) const PROC_THREAD_ATTRIBUTE_MITIGATION_POLICY: usize = 0x0002_0007;

/// Requested sandbox mode for [`spawn_sandboxed`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SandboxRequest {
  /// Preferred mode: AppContainer with zero capabilities.
  AppContainer,
  /// Fallback mode: restricted token + low integrity.
  RestrictedToken,
  /// No token sandboxing (still uses a Job Object for lifecycle guardrails).
  None,
}

/// Configuration for spawning a sandboxed (or unsandboxed) Windows process.
#[derive(Debug, Clone)]
pub struct SpawnConfig {
  /// Path to the executable image.
  pub exe: PathBuf,
  /// Command line args (excluding argv[0]).
  pub args: Vec<OsString>,
  /// Whitelisted handles to inherit into the child process.
  ///
  /// When non-empty, the sandbox uses `PROC_THREAD_ATTRIBUTE_HANDLE_LIST` so only these handles
  /// cross the sandbox boundary.
  pub inherit_handles: Vec<HANDLE>,
  /// Requested sandbox mode.
  pub sandbox: SandboxRequest,
  /// When true and [`SandboxRequest::AppContainer`] fails due to the AppContainer APIs being
  /// unavailable/unsupported, fall back to restricted-token mode.
  pub allow_restricted_token_fallback: bool,
  /// Process creation mitigation policy bitmask (`PROC_THREAD_ATTRIBUTE_MITIGATION_POLICY`).
  pub mitigation_policy: u64,
}

impl SpawnConfig {
  pub fn new(exe: impl Into<PathBuf>) -> Self {
    Self {
      exe: exe.into(),
      args: Vec::new(),
      inherit_handles: Vec::new(),
      sandbox: SandboxRequest::AppContainer,
      allow_restricted_token_fallback: false,
      mitigation_policy: 0,
    }
  }
}

/// A spawned child process along with the Job object used for lifetime management.
#[derive(Debug)]
pub struct ChildProcess {
  pub process: OwnedHandle,
  pub pid: u32,
  pub job: Job,
  pub level: SandboxRequest,
}

impl ChildProcess {
  /// Wait for the process to exit.
  ///
  /// Returns:
  /// - `Ok(None)` if the timeout elapsed
  /// - `Ok(Some(exit_code))` if the process exited
  pub fn wait(&mut self, timeout: Duration) -> Result<Option<u32>> {
    let timeout_ms = timeout.as_millis().min(u32::MAX as u128) as u32;

    // SAFETY: `self.process` is a valid process HANDLE while `ChildProcess` is alive.
    let wait_result = unsafe { WaitForSingleObject(self.process.as_raw(), timeout_ms) };
    if wait_result == WAIT_OBJECT_0 {
      let mut exit_code: u32 = 0;
      // SAFETY: `self.process` is valid.
      let ok = unsafe { GetExitCodeProcess(self.process.as_raw(), &mut exit_code) };
      if ok == 0 {
        return Err(WinSandboxError::last("GetExitCodeProcess"));
      }
      return Ok(Some(exit_code));
    }

    if wait_result == WAIT_TIMEOUT {
      return Ok(None);
    }

    Err(WinSandboxError::last("WaitForSingleObject"))
  }
}

/// Spawn a process according to the requested sandbox configuration.
pub fn spawn_sandboxed(cfg: &SpawnConfig) -> Result<ChildProcess> {
  match cfg.sandbox {
    SandboxRequest::AppContainer => match spawn_appcontainer(cfg) {
      Ok(child) => Ok(child),
      Err(err) => {
        if cfg.allow_restricted_token_fallback && is_appcontainer_unavailable(&err) {
          eprintln!(
            "warning: AppContainer sandbox unavailable ({err}); falling back to restricted-token low-integrity sandbox (network is not reliably blocked)"
          );
          return spawn_restricted_token(cfg);
        }
        Err(err)
      }
    },
    SandboxRequest::RestrictedToken => spawn_restricted_token(cfg),
    SandboxRequest::None => spawn_unsandboxed(cfg),
  }
}

fn spawn_restricted_token(cfg: &SpawnConfig) -> Result<ChildProcess> {
  let token = crate::restricted_token::create_restricted_token_low_integrity()?;

  let policy = effective_mitigation_policy(cfg.mitigation_policy);
  match crate::restricted_token::spawn_with_token(cfg, &token) {
    Ok(child) => Ok(child),
    Err(err) if policy != 0 && should_fallback_without_mitigations(&err) => {
      let mut cfg = cfg.clone();
      cfg.mitigation_policy = 0;
      crate::restricted_token::spawn_with_token(&cfg, &token)
    }
    Err(err) => Err(err),
  }
}

fn spawn_unsandboxed(cfg: &SpawnConfig) -> Result<ChildProcess> {
  let policy = effective_mitigation_policy(cfg.mitigation_policy);
  match spawn_unsandboxed_inner(cfg, policy) {
    Ok(child) => Ok(child),
    Err(err) if policy != 0 && should_fallback_without_mitigations(&err) => {
      spawn_unsandboxed_inner(cfg, 0)
    }
    Err(err) => Err(err),
  }
}

fn spawn_unsandboxed_inner(cfg: &SpawnConfig, mitigation_policy: u64) -> Result<ChildProcess> {
  let job = create_default_job()?;

  let mut mitigation_policy_value = mitigation_policy;
  let mut handles = cfg.inherit_handles.clone();

  let mut attrs: Option<AttributeList> = None;
  let mut attribute_count: u32 = 0;
  if !handles.is_empty() {
    attribute_count += 1;
  }
  if mitigation_policy != 0 {
    attribute_count += 1;
  }
  if attribute_count > 0 {
    let mut list = AttributeList::new(attribute_count)?;
    if !handles.is_empty() {
      list.update_raw(
        PROC_THREAD_ATTRIBUTE_HANDLE_LIST as usize,
        handles.as_mut_ptr().cast(),
        handles.len() * std::mem::size_of::<HANDLE>(),
      )?;
    }
    if mitigation_policy != 0 {
      list.update_raw(
        PROC_THREAD_ATTRIBUTE_MITIGATION_POLICY,
        std::ptr::addr_of_mut!(mitigation_policy_value).cast(),
        std::mem::size_of::<u64>(),
      )?;
    }
    attrs = Some(list);
  }

  let inherit_handles = if handles.is_empty() { FALSE } else { TRUE };
  let pi = create_process_w(cfg, attrs.as_mut(), inherit_handles)?;
  finish_spawn(job, pi, SandboxRequest::None)
}

fn spawn_appcontainer(cfg: &SpawnConfig) -> Result<ChildProcess> {
  let policy = effective_mitigation_policy(cfg.mitigation_policy);
  match spawn_appcontainer_inner(cfg, policy) {
    Ok(child) => Ok(child),
    Err(err) if policy != 0 && should_fallback_without_mitigations(&err) => {
      spawn_appcontainer_inner(cfg, 0)
    }
    Err(err) => Err(err),
  }
}

fn spawn_appcontainer_inner(cfg: &SpawnConfig, mitigation_policy: u64) -> Result<ChildProcess> {
  let job = create_default_job()?;

  let profile = AppContainerProfile::ensure(
    "FastRender.Renderer",
    "FastRender Renderer",
    "FastRender renderer AppContainer profile",
  )?;

  let mut capabilities = SECURITY_CAPABILITIES {
    AppContainerSid: profile.sid().as_ptr(),
    Capabilities: std::ptr::null_mut(),
    CapabilityCount: 0,
    Reserved: 0,
  };

  let mut mitigation_policy_value = mitigation_policy;
  let mut handles = cfg.inherit_handles.clone();

  let mut attribute_count: u32 = 1; // security capabilities
  if !handles.is_empty() {
    attribute_count += 1;
  }
  if mitigation_policy != 0 {
    attribute_count += 1;
  }

  let mut attrs = AttributeList::new(attribute_count)?;
  attrs.update_raw(
    PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES as usize,
    std::ptr::addr_of_mut!(capabilities).cast(),
    std::mem::size_of::<SECURITY_CAPABILITIES>(),
  )?;
  if !handles.is_empty() {
    attrs.update_raw(
      PROC_THREAD_ATTRIBUTE_HANDLE_LIST as usize,
      handles.as_mut_ptr().cast(),
      handles.len() * std::mem::size_of::<HANDLE>(),
    )?;
  }
  if mitigation_policy != 0 {
    attrs.update_raw(
      PROC_THREAD_ATTRIBUTE_MITIGATION_POLICY,
      std::ptr::addr_of_mut!(mitigation_policy_value).cast(),
      std::mem::size_of::<u64>(),
    )?;
  }

  let inherit_handles = if handles.is_empty() { FALSE } else { TRUE };
  let pi = create_process_w(cfg, Some(&mut attrs), inherit_handles)?;
  finish_spawn(job, pi, SandboxRequest::AppContainer)
}

pub(crate) fn effective_mitigation_policy(requested: u64) -> u64 {
  if std::env::var_os("FASTR_DISABLE_WIN_MITIGATIONS").is_some() {
    0
  } else {
    requested
  }
}

pub(crate) fn create_default_job() -> Result<Job> {
  let job = Job::new(None)?;
  job.set_kill_on_close()?;
  job.set_active_process_limit(1)?;
  job.set_ui_restrictions_headless()?;
  Ok(job)
}

fn create_process_w(
  cfg: &SpawnConfig,
  attrs: Option<&mut AttributeList>,
  inherit_handles: i32,
) -> Result<PROCESS_INFORMATION> {
  let application_name = wide_from_os(cfg.exe.as_os_str());
  let mut cmdline = build_command_line(&cfg.exe, &cfg.args);
  let mut pi: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };

  if let Some(attrs) = attrs {
    let mut startup: STARTUPINFOEXW = unsafe { std::mem::zeroed() };
    startup.StartupInfo.cb = std::mem::size_of::<STARTUPINFOEXW>() as u32;
    startup.lpAttributeList = attrs.list;
    let flags = CREATE_SUSPENDED | EXTENDED_STARTUPINFO_PRESENT;
    let ok = unsafe {
      CreateProcessW(
        application_name.as_ptr(),
        cmdline.as_mut_ptr(),
        std::ptr::null(),
        std::ptr::null(),
        inherit_handles,
        flags,
        std::ptr::null(),
        std::ptr::null(),
        std::ptr::addr_of_mut!(startup).cast::<STARTUPINFOW>(),
        &mut pi,
      )
    };
    win32_bool("CreateProcessW", ok)?;
    return Ok(pi);
  }

  let mut startup: STARTUPINFOW = unsafe { std::mem::zeroed() };
  startup.cb = std::mem::size_of::<STARTUPINFOW>() as u32;
  let ok = unsafe {
    CreateProcessW(
      application_name.as_ptr(),
      cmdline.as_mut_ptr(),
      std::ptr::null(),
      std::ptr::null(),
      inherit_handles,
      CREATE_SUSPENDED,
      std::ptr::null(),
      std::ptr::null(),
      &startup,
      &mut pi,
    )
  };
  win32_bool("CreateProcessW", ok)?;
  Ok(pi)
}

pub(crate) fn finish_spawn(job: Job, pi: PROCESS_INFORMATION, level: SandboxRequest) -> Result<ChildProcess> {
  if pi.hProcess.is_null() || pi.hThread.is_null() {
    return Err(WinSandboxError::from_code("CreateProcessW/CreateProcessAsUserW", 0));
  }

  let pid = pi.dwProcessId;
  let process = OwnedHandle::from_raw(pi.hProcess);
  let thread = OwnedHandle::from_raw(pi.hThread);

  if let Err(err) = job.assign_process(&process) {
    unsafe {
      let _ = TerminateProcess(process.as_raw(), 1);
    }
    return Err(err);
  }

  let resume_rc = unsafe { ResumeThread(thread.as_raw()) };
  if resume_rc == u32::MAX {
    let err = WinSandboxError::last("ResumeThread");
    unsafe {
      let _ = TerminateProcess(process.as_raw(), 1);
    }
    return Err(err);
  }
  drop(thread);

  Ok(ChildProcess {
    process,
    pid,
    job,
    level,
  })
}

fn is_appcontainer_unavailable(err: &WinSandboxError) -> bool {
  match err {
    WinSandboxError::Win32 { code, .. } => matches!(
      *code,
      ERROR_NOT_SUPPORTED | ERROR_CALL_NOT_IMPLEMENTED | ERROR_MOD_NOT_FOUND | ERROR_PROC_NOT_FOUND
    ),
    WinSandboxError::HResult { hresult, .. } => {
      // E_NOTIMPL.
      if *hresult == 0x8000_4001 {
        return true;
      }
      // HRESULT_FROM_WIN32.
      if (*hresult & 0xFFFF_0000) == 0x8007_0000 {
        let code = *hresult & 0xFFFF;
        return matches!(
          code,
          ERROR_NOT_SUPPORTED
            | ERROR_CALL_NOT_IMPLEMENTED
            | ERROR_MOD_NOT_FOUND
            | ERROR_PROC_NOT_FOUND
        );
      }
      false
    }
    _ => false,
  }
}

fn should_fallback_without_mitigations(err: &WinSandboxError) -> bool {
  const ERROR_INVALID_PARAMETER: u32 = 87;

  match err {
    WinSandboxError::Win32 { code, .. } => *code == ERROR_INVALID_PARAMETER || *code == ERROR_NOT_SUPPORTED,
    _ => false,
  }
}

fn win32_bool(func: &'static str, value: i32) -> Result<()> {
  if value == 0 {
    Err(WinSandboxError::last(func))
  } else {
    Ok(())
  }
}

pub(crate) fn wide_from_os(value: &OsStr) -> Vec<u16> {
  value.encode_wide().chain(std::iter::once(0)).collect()
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

pub(crate) fn build_command_line(exe: &Path, args: &[OsString]) -> Vec<u16> {
  let mut cmd: Vec<u16> = Vec::new();
  append_arg_escaped(&mut cmd, exe.as_os_str());
  for arg in args {
    cmd.push(b' ' as u16);
    append_arg_escaped(&mut cmd, arg.as_os_str());
  }
  cmd.push(0);
  cmd
}

// -----------------------------------------------------------------------------
// Proc thread attribute list helper.
// -----------------------------------------------------------------------------

pub(crate) struct AttributeList {
  buffer: Vec<u64>,
  pub(crate) list: LPPROC_THREAD_ATTRIBUTE_LIST,
  initialized: bool,
}

impl AttributeList {
  pub(crate) fn new(attribute_count: u32) -> Result<Self> {
    let mut size: usize = 0;

    // Query required size. This call is expected to fail with ERROR_INSUFFICIENT_BUFFER.
    let ok = unsafe {
      InitializeProcThreadAttributeList(std::ptr::null_mut(), attribute_count, 0, &mut size)
    };
    if ok != 0 {
      return Err(WinSandboxError::from_code(
        "InitializeProcThreadAttributeList(size query)",
        0,
      ));
    }
    let err = LastError::last().code();
    if err != ERROR_INSUFFICIENT_BUFFER {
      return Err(WinSandboxError::from_code(
        "InitializeProcThreadAttributeList(size query)",
        err,
      ));
    }
    if size == 0 {
      return Err(WinSandboxError::from_code(
        "InitializeProcThreadAttributeList(size query)",
        0,
      ));
    }

    // Allocate a suitably aligned backing buffer. `STARTUPINFOEX` attribute lists need pointer
    // alignment; `Vec<u64>` provides at least 8-byte alignment on 64-bit builds.
    let word_count = (size + std::mem::size_of::<u64>() - 1) / std::mem::size_of::<u64>();
    let mut buffer = vec![0u64; word_count];
    let list: LPPROC_THREAD_ATTRIBUTE_LIST = buffer.as_mut_ptr().cast();

    let ok = unsafe { InitializeProcThreadAttributeList(list, attribute_count, 0, &mut size) };
    if ok == 0 {
      return Err(WinSandboxError::last("InitializeProcThreadAttributeList"));
    }

    Ok(Self {
      buffer,
      list,
      initialized: true,
    })
  }

  pub(crate) fn update_raw(&mut self, attr: usize, value: *mut c_void, size: usize) -> Result<()> {
    win32_bool("UpdateProcThreadAttribute", unsafe {
      UpdateProcThreadAttribute(
        self.list,
        0,
        attr,
        value,
        size,
        std::ptr::null_mut(),
        std::ptr::null_mut(),
      )
    })
  }
}

impl Drop for AttributeList {
  fn drop(&mut self) {
    if !self.initialized {
      return;
    }

    // SAFETY: If `InitializeProcThreadAttributeList` succeeded, `DeleteProcThreadAttributeList`
    // expects the same pointer.
    unsafe {
      DeleteProcThreadAttributeList(self.list);
    }

    // Keep the buffer field alive until after `DeleteProcThreadAttributeList` (it owns the memory).
    let _ = &self.buffer;
  }
}
