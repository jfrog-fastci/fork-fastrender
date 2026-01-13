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

pub struct RendererSandbox {
  job: Job,
  appcontainer: AppContainerProfile,
  mitigation_policy: u64,
}

pub struct RendererSandboxBuilder {
  allow_unsupported: bool,
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

    Ok(RendererSandbox {
      job,
      appcontainer,
      mitigation_policy: self.mitigation_policy,
    })
  }
}

impl RendererSandbox {
  pub fn new_default() -> Result<Self> {
    RendererSandboxBuilder::new()?.build()
  }

  pub fn builder() -> Result<RendererSandboxBuilder> {
    RendererSandboxBuilder::new()
  }

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
      let mb: u64 = value.parse().map_err(|_| WinSandboxError::InvalidEnvVar {
        name: JOB_MEM_LIMIT_ENV,
        value,
      })?;
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
  block.push(0);
  block
}

fn spawn_windows(
  sandbox: &RendererSandbox,
  exe: PathBuf,
  args: Vec<OsString>,
  inherit_handles: Vec<RawHandle>,
  env: Vec<(OsString, OsString)>,
) -> Result<ChildProcess> {
  use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, HANDLE_FLAG_INHERIT};
  use windows_sys::Win32::System::Threading::{
    CreateProcessW, DeleteProcThreadAttributeList, InitializeProcThreadAttributeList, ResumeThread,
    UpdateProcThreadAttribute, CREATE_SUSPENDED, CREATE_UNICODE_ENVIRONMENT,
    EXTENDED_STARTUPINFO_PRESENT, PROCESS_INFORMATION, PROC_THREAD_ATTRIBUTE_HANDLE_LIST,
    PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES, STARTUPINFOEXW,
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
  let mut cmd_w = build_command_line(&exe, &args);
  let env_block = build_environment_block(env);
  let cwd_w = exe.parent().map(|p| to_wide_null(p.as_os_str()));

  // Build the attribute list.
  let mut attr_count: usize = 1; // mitigation policy
  if sandbox.appcontainer.is_enabled() {
    attr_count += 1;
  }
  if !handles.is_empty() {
    attr_count += 1;
  }

  let mut attr_list_size: usize = 0;
  unsafe {
    InitializeProcThreadAttributeList(std::ptr::null_mut(), attr_count as u32, 0, &mut attr_list_size);
  }

  let mut attr_buf = vec![0u8; attr_list_size];
  let attr_list =
    attr_buf.as_mut_ptr() as windows_sys::Win32::System::Threading::LPPROC_THREAD_ATTRIBUTE_LIST;

  let ok = unsafe { InitializeProcThreadAttributeList(attr_list, attr_count as u32, 0, &mut attr_list_size) };
  if ok == 0 {
    return Err(WinSandboxError::last("InitializeProcThreadAttributeList"));
  }

  struct AttrListGuard(windows_sys::Win32::System::Threading::LPPROC_THREAD_ATTRIBUTE_LIST);
  impl Drop for AttrListGuard {
    fn drop(&mut self) {
      unsafe {
        DeleteProcThreadAttributeList(self.0);
      }
    }
  }
  let _attr_guard = AttrListGuard(attr_list);

  let mut mitigation_policy = crate::spawn::effective_mitigation_policy(sandbox.mitigation_policy);
  let ok = unsafe {
    UpdateProcThreadAttribute(
      attr_list,
      0,
      crate::spawn::PROC_THREAD_ATTRIBUTE_MITIGATION_POLICY,
      (&mut mitigation_policy as *mut u64).cast(),
      std::mem::size_of::<u64>(),
      std::ptr::null_mut(),
      std::ptr::null_mut(),
    )
  };
  if ok == 0 {
    return Err(WinSandboxError::last(
      "UpdateProcThreadAttribute(PROC_THREAD_ATTRIBUTE_MITIGATION_POLICY)",
    ));
  }

  let mut security_capabilities;
  if sandbox.appcontainer.is_enabled() {
    let sid = sandbox
      .appcontainer
      .sid()
      .expect("AppContainerProfile enabled but sid is missing");

    security_capabilities = windows_sys::Win32::Security::SECURITY_CAPABILITIES {
      AppContainerSid: sid.as_ptr() as _,
      Capabilities: std::ptr::null_mut(),
      CapabilityCount: 0,
      Reserved: 0,
    };
    let ok = unsafe {
      UpdateProcThreadAttribute(
        attr_list,
        0,
        PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES as usize,
        (&mut security_capabilities as *mut windows_sys::Win32::Security::SECURITY_CAPABILITIES).cast(),
        std::mem::size_of::<windows_sys::Win32::Security::SECURITY_CAPABILITIES>(),
        std::ptr::null_mut(),
        std::ptr::null_mut(),
      )
    };
    if ok == 0 {
      return Err(WinSandboxError::last(
        "UpdateProcThreadAttribute(PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES)",
      ));
    }
  }

  // Explicit handle inheritance list.
  let mut handles_for_attr: Vec<HANDLE> = handles.clone();
  if !handles.is_empty() {
    let ok = unsafe {
      UpdateProcThreadAttribute(
        attr_list,
        0,
        PROC_THREAD_ATTRIBUTE_HANDLE_LIST as usize,
        handles_for_attr.as_mut_ptr().cast(),
        std::mem::size_of::<HANDLE>() * handles_for_attr.len(),
        std::ptr::null_mut(),
        std::ptr::null_mut(),
      )
    };
    if ok == 0 {
      return Err(WinSandboxError::last(
        "UpdateProcThreadAttribute(PROC_THREAD_ATTRIBUTE_HANDLE_LIST)",
      ));
    }
  }

  let mut si: STARTUPINFOEXW = unsafe { std::mem::zeroed() };
  si.StartupInfo.cb = std::mem::size_of::<STARTUPINFOEXW>() as u32;
  si.lpAttributeList = attr_list;

  let mut pi: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };

  let creation_flags = EXTENDED_STARTUPINFO_PRESENT | CREATE_SUSPENDED | CREATE_UNICODE_ENVIRONMENT;
  let inherit: i32 = if handles.is_empty() { 0 } else { 1 };

  let ok = unsafe {
    CreateProcessW(
      app_w.as_ptr(),
      cmd_w.as_mut_ptr(),
      std::ptr::null(),
      std::ptr::null(),
      inherit,
      creation_flags,
      env_block.as_ptr().cast(),
      cwd_w.as_ref().map(|p| p.as_ptr()).unwrap_or(std::ptr::null()),
      &si.StartupInfo,
      &mut pi,
    )
  };

  if ok == 0 {
    return Err(WinSandboxError::last("CreateProcessW"));
  }

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

  sandbox
    .job
    .assign_process(&RawWin32Handle(proc_cleanup.pi.hProcess))?;

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
