use std::ffi::{OsStr, OsString};
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::{AsRawHandle, RawHandle};
use std::path::PathBuf;

use crate::{AppContainerProfile, Job, Result, WinSandboxError};

pub use crate::child_process::ChildProcess;

const DEFAULT_APPCONTAINER_NAME: &str = "FastRender.Renderer";
const DEFAULT_APPCONTAINER_DISPLAY_NAME: &str = "FastRender Renderer";
const DEFAULT_APPCONTAINER_DESCRIPTION: &str = "FastRender renderer AppContainer profile";

const JOB_MEM_LIMIT_ENV: &str = "FASTR_RENDERER_JOB_MEM_LIMIT_MB";

// STARTUPINFOEX attribute value:
// ProcThreadAttributeValue(15, FALSE, TRUE, FALSE) → 0x0002_000F.
const PROC_THREAD_ATTRIBUTE_ALL_APPLICATION_PACKAGES_POLICY: usize = 0x0002_000F;

// Value for `PROC_THREAD_ATTRIBUTE_ALL_APPLICATION_PACKAGES_POLICY` (winbase.h).
const PROCESS_CREATION_ALL_APPLICATION_PACKAGES_POLICY_BLOCK: u32 = 1;

pub struct RendererSandbox {
  job: Job,
  appcontainer: AppContainerProfile,
  mitigation_policy: u64,
  allow_jobless: bool,
}

pub struct RendererSandboxBuilder {
  allow_unsupported: bool,
  allow_jobless: bool,
  appcontainer_name: String,
  appcontainer_display_name: String,
  appcontainer_description: String,
  mitigation_policy: u64,
  active_process_limit: u32,
  job_memory_limit_bytes: Option<usize>,
}

impl RendererSandboxBuilder {
  pub fn new() -> Result<Self> {
    Ok(Self {
      allow_unsupported: false,
      allow_jobless: false,
      appcontainer_name: DEFAULT_APPCONTAINER_NAME.to_owned(),
      appcontainer_display_name: DEFAULT_APPCONTAINER_DISPLAY_NAME.to_owned(),
      appcontainer_description: DEFAULT_APPCONTAINER_DESCRIPTION.to_owned(),
      mitigation_policy: crate::mitigations::renderer_mitigation_policy(),
      active_process_limit: 1,
      job_memory_limit_bytes: parse_job_mem_limit_env()?,
    })
  }

  pub fn allow_unsupported(mut self, allow: bool) -> Self {
    self.allow_unsupported = allow;
    self
  }

  pub fn allow_jobless(mut self, allow: bool) -> Self {
    self.allow_jobless = allow;
    self
  }

  pub fn mitigation_policy(mut self, policy: u64) -> Self {
    self.mitigation_policy = policy;
    self
  }

  pub fn job_active_process_limit(mut self, limit: u32) -> Self {
    self.active_process_limit = limit;
    self
  }

  pub fn job_memory_limit_bytes(mut self, limit: Option<usize>) -> Self {
    self.job_memory_limit_bytes = limit;
    self
  }

  pub fn appcontainer_name(mut self, name: impl Into<String>) -> Self {
    self.appcontainer_name = name.into();
    self
  }

  pub fn appcontainer_display_name(mut self, display_name: impl Into<String>) -> Self {
    self.appcontainer_display_name = display_name.into();
    self
  }

  pub fn appcontainer_description(mut self, description: impl Into<String>) -> Self {
    self.appcontainer_description = description.into();
    self
  }

  pub fn build(self) -> Result<RendererSandbox> {
    let appcontainer = match AppContainerProfile::ensure(
      &self.appcontainer_name,
      &self.appcontainer_display_name,
      &self.appcontainer_description,
    ) {
      Ok(v) => v,
      Err(_) if self.allow_unsupported => AppContainerProfile::disabled(),
      Err(e) => return Err(e),
    };

    let job = Job::new(None)?;
    job.set_kill_on_close()?;
    job.set_active_process_limit(self.active_process_limit)?;
    if let Some(limit) = self.job_memory_limit_bytes {
      job.set_job_memory_limit_bytes(limit)?;
    }
    job.set_ui_restrictions_headless()?;

    Ok(RendererSandbox {
      job,
      appcontainer,
      mitigation_policy: self.mitigation_policy,
      allow_jobless: self.allow_jobless,
    })
  }
}

impl RendererSandbox {
  pub fn new_default() -> Result<Self> {
    let support = crate::SandboxSupport::detect();
    if support != crate::SandboxSupport::Full && !crate::allow_unsandboxed_renderer() {
      return Err(crate::RendererSandboxModeError::Unsupported { support }.into());
    }

    // If the caller explicitly allows running without the full sandbox, configure a best-effort
    // sandbox that can degrade to jobless/AppContainer-disabled mode if setup fails.
    let allow_fallback = crate::allow_unsandboxed_renderer();
    let mut builder = RendererSandboxBuilder::new()?;
    if allow_fallback {
      builder = builder.allow_unsupported(true).allow_jobless(true);
    }
    builder.build()
  }

  pub fn builder() -> Result<RendererSandboxBuilder> {
    RendererSandboxBuilder::new()
  }

  /// Spawns a child process under the configured renderer-style sandbox.
  ///
  /// Note: `env` is an override list applied on top of the current process environment; this helper
  /// does **not** perform environment sanitization.
  pub fn spawn(
    &self,
    exe: PathBuf,
    args: Vec<OsString>,
    inherit_handles: Vec<RawHandle>,
    env: Vec<(OsString, OsString)>,
  ) -> Result<ChildProcess> {
    spawn_windows(self, exe, args, inherit_handles, env)
  }
}

fn parse_job_mem_limit_env() -> Result<Option<usize>> {
  match std::env::var(JOB_MEM_LIMIT_ENV) {
    Ok(value) => {
      let trimmed = value.trim();
      if trimmed.is_empty() || trimmed == "0" {
        return Ok(None);
      }

      // Allow `_` separators for readability (e.g. `1_024`).
      let normalized: String = trimmed.chars().filter(|c| *c != '_').collect();
      let mb: u64 = normalized.parse().map_err(|_| WinSandboxError::InvalidEnvVar {
        name: JOB_MEM_LIMIT_ENV,
        value,
      })?;
      if mb == 0 {
        return Ok(None);
      }
      let bytes = mb.saturating_mul(1024 * 1024);
      Ok(Some(bytes.min(usize::MAX as u64) as usize))
    }
    Err(std::env::VarError::NotPresent) => Ok(None),
    Err(std::env::VarError::NotUnicode(_)) => Err(WinSandboxError::InvalidEnvVar {
      name: JOB_MEM_LIMIT_ENV,
      value: "<non-unicode>".to_owned(),
    }),
  }
}

fn to_wide_null(s: &OsStr) -> Vec<u16> {
  s.encode_wide().chain([0]).collect()
}

fn append_cmdline_arg(out: &mut Vec<u16>, arg: &OsStr) {
  const QUOTE: u16 = b'"' as u16;
  const BACKSLASH: u16 = b'\\' as u16;
  const SPACE: u16 = b' ' as u16;
  const TAB: u16 = b'\t' as u16;
  const NEWLINE: u16 = b'\n' as u16;

  let needs_quotes = {
    let mut it = arg.encode_wide();
    match it.next() {
      None => true,
      Some(first) => {
        if first == QUOTE {
          true
        } else {
          let mut needs = false;
          for ch in std::iter::once(first).chain(it) {
            if ch == SPACE || ch == TAB || ch == NEWLINE || ch == QUOTE {
              needs = true;
              break;
            }
          }
          needs
        }
      }
    }
  };

  if !needs_quotes {
    out.extend(arg.encode_wide());
    return;
  }

  out.push(QUOTE);
  let mut backslashes = 0usize;
  for ch in arg.encode_wide() {
    match ch {
      BACKSLASH => backslashes += 1,
      QUOTE => {
        out.extend(std::iter::repeat(BACKSLASH).take(backslashes * 2 + 1));
        out.push(QUOTE);
        backslashes = 0;
      }
      _ => {
        if backslashes != 0 {
          out.extend(std::iter::repeat(BACKSLASH).take(backslashes));
          backslashes = 0;
        }
        out.push(ch);
      }
    }
  }

  if backslashes != 0 {
    out.extend(std::iter::repeat(BACKSLASH).take(backslashes * 2));
  }
  out.push(QUOTE);
}

fn build_command_line(exe: &PathBuf, args: &[OsString]) -> Vec<u16> {
  let mut out = Vec::<u16>::new();
  append_cmdline_arg(&mut out, exe.as_os_str());
  for arg in args {
    out.push(b' ' as u16);
    append_cmdline_arg(&mut out, arg.as_os_str());
  }
  out.push(0);
  out
}

fn build_environment_block(overrides: Vec<(OsString, OsString)>) -> Vec<u16> {
  let mut vars: Vec<(OsString, OsString)> = std::env::vars_os().collect();

  // Apply overrides (case-insensitive on Windows).
  for (k, v) in overrides {
    let key_norm = k.to_string_lossy().to_ascii_uppercase();
    vars.retain(|(ek, _)| ek.to_string_lossy().to_ascii_uppercase() != key_norm);
    vars.push((k, v));
  }

  vars.sort_by(|(a, _), (b, _)| {
    a.to_string_lossy()
      .to_ascii_uppercase()
      .cmp(&b.to_string_lossy().to_ascii_uppercase())
  });

  let mut block = Vec::<u16>::new();
  for (k, v) in vars {
    block.extend(k.encode_wide());
    block.push(b'=' as u16);
    block.extend(v.encode_wide());
    block.push(0);
  }
  // Environment blocks are double-NUL terminated. When there are no variables, this must still be
  // two NULs (not one).
  block.push(0);
  if block.len() == 1 {
    block.push(0);
  }
  block
}

fn spawn_windows(
  sandbox: &RendererSandbox,
  exe: PathBuf,
  args: Vec<OsString>,
  inherit_handles: Vec<RawHandle>,
  env: Vec<(OsString, OsString)>,
) -> Result<ChildProcess> {
  use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, HANDLE_FLAG_INHERIT, ERROR_ACCESS_DENIED};
  use windows_sys::Win32::System::JobObjects::IsProcessInJob;
  use windows_sys::Win32::System::Threading::{
    CreateProcessW, GetCurrentProcess, ResumeThread, CREATE_BREAKAWAY_FROM_JOB, CREATE_SUSPENDED,
    CREATE_UNICODE_ENVIRONMENT, EXTENDED_STARTUPINFO_PRESENT, PROCESS_INFORMATION,
    PROC_THREAD_ATTRIBUTE_HANDLE_LIST,
    PROC_THREAD_ATTRIBUTE_MITIGATION_POLICY, PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES,
    STARTUPINFOEXW, STARTUPINFOW,
  };

  struct RawWin32Handle(HANDLE);
  impl AsRawHandle for RawWin32Handle {
    fn as_raw_handle(&self) -> RawHandle {
      self.0 as RawHandle
    }
  }

  // Prepare handles for inheritance (only the explicitly listed ones).
  let handles: Vec<HANDLE> = inherit_handles
    .iter()
    .copied()
    .map(|h| h as HANDLE)
    .collect();

  // Ensure listed handles are inheritable.
  let mut made_inheritable: Vec<HANDLE> = Vec::new();
  for &h in &handles {
    if h.is_null() {
      continue;
    }

    let mut flags: u32 = 0;
    let ok = unsafe { windows_sys::Win32::Foundation::GetHandleInformation(h, &mut flags) };
    if ok == 0 {
      return Err(WinSandboxError::last("GetHandleInformation"));
    }

    if (flags & HANDLE_FLAG_INHERIT) == 0 {
      let ok = unsafe {
        windows_sys::Win32::Foundation::SetHandleInformation(h, HANDLE_FLAG_INHERIT, HANDLE_FLAG_INHERIT)
      };
      if ok == 0 {
        return Err(WinSandboxError::last("SetHandleInformation"));
      }
      made_inheritable.push(h);
    }
  }

  // Ensure we reset the inherit flag even if we early-return.
  struct RestoreInheritFlags(Vec<HANDLE>);
  impl Drop for RestoreInheritFlags {
    fn drop(&mut self) {
      for &h in &self.0 {
        unsafe {
          let _ = windows_sys::Win32::Foundation::SetHandleInformation(h, HANDLE_FLAG_INHERIT, 0);
        }
      }
    }
  }
  let _restore = RestoreInheritFlags(made_inheritable);

  let app_w = to_wide_null(exe.as_os_str());
  let env_block = build_environment_block(env);
  let cwd_w = exe.parent().map(|p| to_wide_null(p.as_os_str()));

  let mitigation_policy = if sandbox.mitigation_policy != 0
    && std::env::var_os("FASTR_DISABLE_WIN_MITIGATIONS").is_some()
  {
    0
  } else {
    sandbox.mitigation_policy
  };

  let parent_in_job = {
    let mut in_job: i32 = 0;
    // SAFETY: `in_job` is a valid out param; null job handle queries "any job".
    let ok = unsafe { IsProcessInJob(GetCurrentProcess(), std::ptr::null_mut(), &mut in_job) };
    ok != 0 && in_job != 0
  };

  fn should_fallback_without_mitigations(err: &WinSandboxError) -> bool {
    const ERROR_INVALID_PARAMETER: u32 = 87;
    const ERROR_NOT_SUPPORTED: u32 = windows_sys::Win32::Foundation::ERROR_NOT_SUPPORTED;
    match err {
      WinSandboxError::Win32 { code, .. } => {
        *code == ERROR_INVALID_PARAMETER || *code == ERROR_NOT_SUPPORTED
      }
      _ => false,
    }
  }

  let is_optional_attr_error = should_fallback_without_mitigations;

  let create_process_once =
    |mitigation_policy: u64, include_aap: bool| -> Result<PROCESS_INFORMATION> {
      let mut cmd_w = build_command_line(&exe, &args);
      let mut pi: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };

      let mut attr_count: u32 = 0;
      if sandbox.appcontainer.is_enabled() {
        attr_count += 1; // SECURITY_CAPABILITIES
        if include_aap {
          attr_count += 1;
        }
      }
      if !handles.is_empty() {
        attr_count += 1;
      }
      if mitigation_policy != 0 {
        attr_count += 1;
      }

      let cwd_ptr = cwd_w
        .as_ref()
        .map(|p| p.as_ptr())
        .unwrap_or(std::ptr::null());

      if attr_count == 0 {
        let mut si: STARTUPINFOW = unsafe { std::mem::zeroed() };
        si.cb = std::mem::size_of::<STARTUPINFOW>() as u32;

        let mut create_process_inner = |flags: u32| -> Result<()> {
          cmd_w = build_command_line(&exe, &args);
          let ok = unsafe {
            CreateProcessW(
              app_w.as_ptr(),
              cmd_w.as_mut_ptr(),
              std::ptr::null(),
              std::ptr::null(),
              0,
              flags,
              env_block.as_ptr().cast(),
              cwd_ptr,
              &mut si,
              &mut pi,
            )
          };
          if ok == 0 {
            return Err(WinSandboxError::last("CreateProcessW"));
          }
          Ok(())
        };

        let flags = CREATE_SUSPENDED | CREATE_UNICODE_ENVIRONMENT;
        if parent_in_job {
          match create_process_inner(flags | CREATE_BREAKAWAY_FROM_JOB) {
            Ok(()) => {}
            Err(err)
              if matches!(err, WinSandboxError::Win32 { code, .. } if code == ERROR_ACCESS_DENIED) =>
            {
              create_process_inner(flags)?;
            }
            Err(err) => return Err(err),
          }
        } else {
          create_process_inner(flags)?;
        }
        return Ok(pi);
      }

      let mut handles_for_attr: Vec<HANDLE> = handles.clone();
      let mut mitigation_policy_value = mitigation_policy;
      let mut all_packages_policy_value =
        PROCESS_CREATION_ALL_APPLICATION_PACKAGES_POLICY_BLOCK;
      let mut security_capabilities;

      let mut attrs = crate::spawn::AttributeList::new(attr_count)?;

      if sandbox.appcontainer.is_enabled() {
        let sid = sandbox.appcontainer.sid();
        security_capabilities = windows_sys::Win32::Security::SECURITY_CAPABILITIES {
          AppContainerSid: sid.as_ptr(),
          Capabilities: std::ptr::null_mut(),
          CapabilityCount: 0,
          Reserved: 0,
        };
        attrs.update(
          PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES as usize,
          std::ptr::addr_of_mut!(security_capabilities).cast(),
          std::mem::size_of::<windows_sys::Win32::Security::SECURITY_CAPABILITIES>(),
        )?;

        if include_aap {
          if let Err(err) = attrs.update(
            PROC_THREAD_ATTRIBUTE_ALL_APPLICATION_PACKAGES_POLICY,
            std::ptr::addr_of_mut!(all_packages_policy_value).cast(),
            std::mem::size_of::<u32>(),
          ) {
            if is_optional_attr_error(&err) {
              // Best-effort: older Windows builds may reject this attribute.
            } else {
              return Err(err);
            }
          }
        }
      }

      if !handles.is_empty() {
        attrs.update(
          PROC_THREAD_ATTRIBUTE_HANDLE_LIST as usize,
          handles_for_attr.as_mut_ptr().cast(),
          std::mem::size_of::<HANDLE>() * handles_for_attr.len(),
        )?;
      }

      if mitigation_policy != 0 {
        attrs.update(
          PROC_THREAD_ATTRIBUTE_MITIGATION_POLICY as usize,
          std::ptr::addr_of_mut!(mitigation_policy_value).cast(),
          std::mem::size_of::<u64>(),
        )?;
      }

      let mut si: STARTUPINFOEXW = unsafe { std::mem::zeroed() };
      si.StartupInfo.cb = std::mem::size_of::<STARTUPINFOEXW>() as u32;
      si.lpAttributeList = attrs.ptr;

      let inherit: i32 = if handles.is_empty() { 0 } else { 1 };
      let mut create_process_inner = |flags: u32| -> Result<()> {
        cmd_w = build_command_line(&exe, &args);
        let ok = unsafe {
          CreateProcessW(
            app_w.as_ptr(),
            cmd_w.as_mut_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            inherit,
            flags,
            env_block.as_ptr().cast(),
            cwd_ptr,
            &mut si.StartupInfo,
            &mut pi,
          )
        };
        if ok == 0 {
          return Err(WinSandboxError::last("CreateProcessW"));
        }
        Ok(())
      };

      let flags = EXTENDED_STARTUPINFO_PRESENT | CREATE_SUSPENDED | CREATE_UNICODE_ENVIRONMENT;
      if parent_in_job {
        match create_process_inner(flags | CREATE_BREAKAWAY_FROM_JOB) {
          Ok(()) => {}
          Err(err)
            if matches!(err, WinSandboxError::Win32 { code, .. } if code == ERROR_ACCESS_DENIED) =>
          {
            create_process_inner(flags)?;
          }
          Err(err) => return Err(err),
        }
      } else {
        create_process_inner(flags)?;
      }
      Ok(pi)
    };

  let create_process = |mitigation_policy: u64| -> Result<PROCESS_INFORMATION> {
    if !sandbox.appcontainer.is_enabled() {
      return create_process_once(mitigation_policy, false);
    }

    match create_process_once(mitigation_policy, true) {
      Ok(pi) => Ok(pi),
      Err(err) if is_optional_attr_error(&err) => create_process_once(mitigation_policy, false),
      Err(err) => Err(err),
    }
  };

  let pi = match create_process(mitigation_policy) {
    Ok(pi) => pi,
    Err(err) if mitigation_policy != 0 && is_optional_attr_error(&err) => {
      create_process(0)?
    }
    Err(err) => return Err(err),
  };

  struct ProcCleanup {
    pi: PROCESS_INFORMATION,
    cleanup: bool,
  }
  impl Drop for ProcCleanup {
    fn drop(&mut self) {
      if !self.cleanup {
        return;
      }
      unsafe {
        let _ = windows_sys::Win32::System::Threading::TerminateProcess(self.pi.hProcess, 1);
        CloseHandle(self.pi.hThread);
        CloseHandle(self.pi.hProcess);
      }
    }
  }
  let mut proc_cleanup = ProcCleanup { pi, cleanup: true };

  if let Err(err) = sandbox
    .job
    .assign_process(&RawWin32Handle(proc_cleanup.pi.hProcess))
  {
    if sandbox.allow_jobless {
      eprintln!(
        "warning: win-sandbox renderer: AssignProcessToJobObject failed ({err}); continuing without job limits (kill-on-close + active-process cap)"
      );
    } else {
      return Err(err);
    }
  }

  let resume = unsafe { ResumeThread(proc_cleanup.pi.hThread) };
  if resume == u32::MAX {
    return Err(WinSandboxError::last("ResumeThread"));
  }

  proc_cleanup.cleanup = false;

  Ok(ChildProcess::new(
    proc_cleanup.pi.hProcess,
    proc_cleanup.pi.hThread,
    proc_cleanup.pi.dwProcessId,
  ))
}
