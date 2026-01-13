//! Restricted-token sandbox fallback for Windows.
//!
//! This is a **best-effort** sandbox used when AppContainer is unavailable. The intent is to reduce
//! the integrity level (Low) and strip powerful groups/privileges from the primary token.
//!
//! ## Limitations
//!
//! A restricted token **does not reliably block network access** on Windows. If you need strong
//! network isolation, prefer **AppContainer** (no capabilities) when available.

#![cfg(windows)]

use crate::spawn::{build_command_line, build_environment_block, wide_null, AttributeList};
use crate::{ChildProcess, OwnedHandle, OwnedSid, Result, SpawnConfig, WinSandboxError};

use windows_sys::Win32::Foundation::{
  ERROR_ACCESS_DENIED, ERROR_INVALID_PARAMETER, ERROR_NOT_SUPPORTED, FALSE, HANDLE, TRUE,
};
use windows_sys::Win32::Security::Authorization::ConvertStringSidToSidW;
use windows_sys::Win32::Security::{
  CreateRestrictedToken, GetLengthSid, SetTokenInformation, TokenIntegrityLevel,
  DISABLE_MAX_PRIVILEGE, PSID, SID_AND_ATTRIBUTES, TOKEN_ADJUST_DEFAULT, TOKEN_ASSIGN_PRIMARY,
  TOKEN_DUPLICATE, TOKEN_INFORMATION_CLASS, TOKEN_MANDATORY_LABEL, TOKEN_QUERY,
};
use windows_sys::Win32::System::JobObjects::IsProcessInJob;
use windows_sys::Win32::System::SystemServices::{SE_GROUP_INTEGRITY, SE_GROUP_INTEGRITY_ENABLED};
use windows_sys::Win32::System::Threading::{
  CreateProcessAsUserW, GetCurrentProcess, OpenProcessToken, CREATE_BREAKAWAY_FROM_JOB,
  CREATE_UNICODE_ENVIRONMENT, EXTENDED_STARTUPINFO_PRESENT, PROCESS_INFORMATION,
  PROC_THREAD_ATTRIBUTE_HANDLE_LIST, PROC_THREAD_ATTRIBUTE_JOB_LIST,
  PROC_THREAD_ATTRIBUTE_MITIGATION_POLICY, STARTUPINFOEXW, STARTUPINFOW,
};

/// A restricted primary token suitable for spawning a sandboxed child process.
///
/// This is the "fallback" Windows sandbox mode used when AppContainer is unavailable.
#[derive(Debug)]
pub struct RestrictedToken {
  token: OwnedHandle,
}

impl RestrictedToken {
  /// Creates a restricted token derived from the current process token.
  ///
  /// The returned token is suitable for use as a primary token in `CreateProcessAsUserW`.
  pub fn for_current_process_low_integrity() -> Result<Self> {
    Ok(Self {
      token: create_restricted_token_low_integrity()?,
    })
  }

  /// Returns the underlying Win32 `HANDLE` for the token.
  pub fn handle(&self) -> HANDLE {
    self.token.as_raw()
  }

  /// Consumes the wrapper and returns the underlying owned handle.
  pub fn into_handle(self) -> OwnedHandle {
    self.token
  }
}

/// Create a restricted primary token and set its integrity level to Low.
///
/// The returned token is intended for spawning a child process via `CreateProcessAsUserW`.
///
/// # Disabled SIDs
///
/// We disable (mark as *deny-only*) a conservative set of well-known groups that commonly grant
/// broad local privileges:
///
/// - `S-1-5-32-544` (`BUILTIN\\Administrators`)
/// - `S-1-5-32-547` (`BUILTIN\\Power Users`)
/// - `S-1-5-32-545` (`BUILTIN\\Users`)
///
/// This is defense-in-depth: many filesystem ACLs grant read/execute via these groups, so disabling
/// them reduces the chance of the renderer being able to traverse into sensitive locations like the
/// user profile.
pub fn create_restricted_token_low_integrity() -> Result<OwnedHandle> {
  let mut token: HANDLE = std::ptr::null_mut();
  win32_bool("OpenProcessToken", unsafe {
    OpenProcessToken(
      GetCurrentProcess(),
      TOKEN_DUPLICATE | TOKEN_QUERY | TOKEN_ASSIGN_PRIMARY | TOKEN_ADJUST_DEFAULT,
      &mut token,
    )
  })?;
  if token.is_null() {
    return Err(WinSandboxError::NullPointer {
      func: "OpenProcessToken",
    });
  }
  let token = OwnedHandle::from_raw(token);

  // Well-known group SIDs we want to disable.
  let admin_sid = sid_from_sddl("S-1-5-32-544")?;
  let power_users_sid = sid_from_sddl("S-1-5-32-547")?;
  let users_sid = sid_from_sddl("S-1-5-32-545")?;

  // `CreateRestrictedToken` requires disabled SIDs to be present in the existing token on some
  // Windows builds/configurations. To keep this reliable across user accounts (for example a
  // standard user that is not in `BUILTIN\\Administrators` or `BUILTIN\\Power Users`), we attempt a
  // conservative sequence.
  //
  // This still satisfies the intent: if the current token contains any of these well-known groups,
  // we disable them; otherwise there is nothing to disable.
  let attempts: [&[&OwnedSid]; 4] = [
    &[&admin_sid, &power_users_sid, &users_sid],
    &[&admin_sid, &users_sid],
    &[&users_sid],
    &[],
  ];

  let mut last_err: Option<WinSandboxError> = None;
  let mut restricted: Option<OwnedHandle> = None;
  for disabled in attempts {
    match create_restricted_token_disable_sids(token.as_raw(), disabled) {
      Ok(tok) => {
        restricted = Some(tok);
        break;
      }
      Err(err) => {
        if !should_retry_disabled_sids(&err) {
          return Err(err);
        }
        last_err = Some(err);
      }
    }
  }

  let restricted = match restricted {
    Some(tok) => tok,
    None => {
      return Err(
        last_err.unwrap_or_else(|| WinSandboxError::from_code("CreateRestrictedToken", 0)),
      )
    }
  };

  set_low_integrity(restricted.as_raw())?;
  Ok(restricted)
}

/// Spawn a child process with the provided primary token via `CreateProcessAsUserW`.
///
/// Note: `cfg.env` is an override list applied on top of the current process environment; this
/// helper does **not** perform environment sanitization.
///
/// ## Parent already in a Job object
///
/// When `cfg.job` is set and the current process is already running inside a Windows Job, the OS
/// may require the child to be created with `CREATE_BREAKAWAY_FROM_JOB` (depending on nested-job
/// support + parent job policy). In that case this helper:
///
/// 1. Tries `CreateProcessAsUserW` with `CREATE_BREAKAWAY_FROM_JOB`.
/// 2. Retries without breakaway if the breakaway attempt fails with `ERROR_ACCESS_DENIED`.
///
/// This is best-effort compatibility: it does not provide a "jobless" fallback mode.
pub fn spawn_with_token(cfg: &SpawnConfig<'_>, token: &OwnedHandle) -> Result<ChildProcess> {
  // Match the behavior of `spawn_sandboxed`: mitigations can be disabled via an env var.
  let mitigation_policy = match cfg.mitigation_policy {
    Some(bits) if bits != 0 && std::env::var_os("FASTR_DISABLE_WIN_MITIGATIONS").is_none() => {
      Some(bits)
    }
    _ => None,
  };

  fn mitigation_policy_attribute_unsupported(err: &WinSandboxError) -> bool {
    match err {
      WinSandboxError::Win32 { code, .. } => {
        *code == ERROR_INVALID_PARAMETER || *code == ERROR_NOT_SUPPORTED
      }
      _ => false,
    }
  }

  match spawn_with_token_inner(cfg, token, mitigation_policy) {
    Ok(child) => Ok(child),
    Err(err) if mitigation_policy.is_some() && mitigation_policy_attribute_unsupported(&err) => {
      spawn_with_token_inner(cfg, token, None)
    }
    Err(err) => Err(err),
  }
}

fn spawn_with_token_inner(
  cfg: &SpawnConfig<'_>,
  token: &OwnedHandle,
  mitigation_policy: Option<u64>,
) -> Result<ChildProcess> {
  let application_name = wide_null(cfg.exe.as_os_str());
  let mut cmdline = build_command_line(cfg.exe.as_os_str(), &cfg.args);

  let env_block = build_environment_block(&cfg.env);
  let env_ptr = env_block
    .as_ref()
    .map(|b| b.as_ptr() as *const core::ffi::c_void)
    .unwrap_or(std::ptr::null());

  // If `lpCurrentDirectory` is NULL, Windows inherits the parent's current directory. For low
  // integrity/restricted tokens that directory may be inaccessible (e.g. a dev checkout that is not
  // readable at Low IL), causing `CreateProcessAsUserW` to fail with `ERROR_ACCESS_DENIED`.
  //
  // Prefer an explicit `cfg.current_dir` if provided. Otherwise, use the executable's parent
  // directory as a best-effort choice: if the executable is loadable, the directory is generally
  // traversable too.
  let current_dir_w;
  let current_dir_ptr = if let Some(dir) = &cfg.current_dir {
    current_dir_w = wide_null(dir.as_os_str());
    current_dir_w.as_ptr()
  } else if let Some(parent) = cfg.exe.parent() {
    current_dir_w = wide_null(parent.as_os_str());
    current_dir_w.as_ptr()
  } else {
    std::ptr::null()
  };

  let b_inherit_handles = if cfg.inherit_handles.is_empty() {
    FALSE
  } else {
    TRUE
  };

  let needs_job = cfg.job.is_some();
  let needs_handle_list = !cfg.inherit_handles.is_empty();
  let needs_mitigation = mitigation_policy.is_some();
  let attribute_count = (needs_job as u32) + (needs_handle_list as u32) + (needs_mitigation as u32);

  let parent_in_job = if needs_job {
    let mut in_job: i32 = 0;
    // SAFETY: `in_job` is a valid out param; null job handle queries "any job".
    let ok = unsafe { IsProcessInJob(GetCurrentProcess(), std::ptr::null_mut(), &mut in_job) };
    ok != 0 && in_job != 0
  } else {
    false
  };

  // Attribute values must live until after CreateProcessAsUserW returns.
  let mut inherit_handle_list: Vec<HANDLE> =
    cfg.inherit_handles.iter().map(|&h| h as HANDLE).collect();

  let mut job_handle: HANDLE = std::ptr::null_mut();
  if let Some(job) = cfg.job {
    job_handle = job.handle();
  }

  let mut mitigation_policy_value: u64 = mitigation_policy.unwrap_or(0);

  let mut pi: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };

  if attribute_count == 0 {
    let mut startup: STARTUPINFOW = unsafe { std::mem::zeroed() };
    startup.cb = std::mem::size_of::<STARTUPINFOW>() as u32;
    let mut create_process_inner = |flags: u32| -> Result<()> {
      let ok = unsafe {
        CreateProcessAsUserW(
          token.as_raw(),
          application_name.as_ptr(),
          cmdline.as_mut_ptr(),
          std::ptr::null(),
          std::ptr::null(),
          b_inherit_handles,
          flags,
          env_ptr as *const _ as *mut _,
          current_dir_ptr,
          &startup,
          &mut pi,
        )
      };
      win32_bool("CreateProcessAsUserW", ok)
    };

    let flags = CREATE_UNICODE_ENVIRONMENT;
    if parent_in_job {
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
  } else {
    let mut attrs = AttributeList::new(attribute_count)?;
    if needs_job {
      attrs.update(
        PROC_THREAD_ATTRIBUTE_JOB_LIST as usize,
        (&mut job_handle as *mut HANDLE).cast(),
        std::mem::size_of::<HANDLE>(),
      )?;
    }
    if needs_handle_list {
      attrs.update(
        PROC_THREAD_ATTRIBUTE_HANDLE_LIST as usize,
        inherit_handle_list.as_mut_ptr().cast(),
        inherit_handle_list.len() * std::mem::size_of::<HANDLE>(),
      )?;
    }
    if needs_mitigation {
      attrs.update(
        PROC_THREAD_ATTRIBUTE_MITIGATION_POLICY as usize,
        (&mut mitigation_policy_value as *mut u64).cast(),
        std::mem::size_of::<u64>(),
      )?;
    }

    let mut startup: STARTUPINFOEXW = unsafe { std::mem::zeroed() };
    startup.StartupInfo.cb = std::mem::size_of::<STARTUPINFOEXW>() as u32;
    startup.lpAttributeList = attrs.ptr;
    let flags = CREATE_UNICODE_ENVIRONMENT | EXTENDED_STARTUPINFO_PRESENT;
    let mut create_process_inner = |flags: u32| -> Result<()> {
      let ok = unsafe {
        CreateProcessAsUserW(
          token.as_raw(),
          application_name.as_ptr(),
          cmdline.as_mut_ptr(),
          std::ptr::null(),
          std::ptr::null(),
          b_inherit_handles,
          flags,
          env_ptr as *const _ as *mut _,
          current_dir_ptr,
          std::ptr::addr_of_mut!(startup).cast::<STARTUPINFOW>(),
          &mut pi,
        )
      };
      win32_bool("CreateProcessAsUserW", ok)
    };

    if parent_in_job {
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
  }

  if pi.hProcess.is_null() || pi.hThread.is_null() {
    unsafe {
      if !pi.hThread.is_null() {
        windows_sys::Win32::Foundation::CloseHandle(pi.hThread);
      }
      if !pi.hProcess.is_null() {
        windows_sys::Win32::Foundation::CloseHandle(pi.hProcess);
      }
    }
    return Err(WinSandboxError::NullPointer {
      func: "CreateProcessAsUserW",
    });
  }

  Ok(ChildProcess::new(pi.hProcess, pi.hThread))
}

fn set_low_integrity(token: HANDLE) -> Result<()> {
  // Low integrity SID: S-1-16-4096.
  let sid = sid_from_sddl("S-1-16-4096")?;

  let sid_len = unsafe { GetLengthSid(sid.as_ptr()) } as usize;
  let tml_len = std::mem::size_of::<TOKEN_MANDATORY_LABEL>() + sid_len;

  // `TOKEN_MANDATORY_LABEL` contains pointers, so ensure the backing buffer has at least pointer
  // alignment. `Vec<u8>` only guarantees 1-byte alignment.
  let word_count = (tml_len + std::mem::size_of::<usize>() - 1) / std::mem::size_of::<usize>();
  let mut buffer_words = vec![0usize; word_count];
  let buffer_ptr = buffer_words.as_mut_ptr().cast::<u8>();

  let tml_ptr = buffer_ptr.cast::<TOKEN_MANDATORY_LABEL>();
  let sid_ptr = unsafe { buffer_ptr.add(std::mem::size_of::<TOKEN_MANDATORY_LABEL>()) };
  unsafe {
    (*tml_ptr).Label.Attributes = (SE_GROUP_INTEGRITY | SE_GROUP_INTEGRITY_ENABLED) as u32;
    (*tml_ptr).Label.Sid = sid_ptr.cast();
    std::ptr::copy_nonoverlapping(sid.as_ptr().cast::<u8>(), sid_ptr, sid_len);
  }

  // SAFETY: `buffer_ptr` points to a valid `TOKEN_MANDATORY_LABEL` followed by an integrity SID.
  let ok = unsafe {
    SetTokenInformation(
      token,
      TokenIntegrityLevel as TOKEN_INFORMATION_CLASS,
      buffer_ptr.cast(),
      tml_len as u32,
    )
  };
  win32_bool("SetTokenInformation(TokenIntegrityLevel)", ok)
}

fn sid_from_sddl(sddl: &str) -> Result<OwnedSid> {
  let wide = wide_from_str(sddl);
  let mut sid: PSID = std::ptr::null_mut();
  win32_bool("ConvertStringSidToSidW", unsafe {
    ConvertStringSidToSidW(wide.as_ptr(), &mut sid)
  })?;
  if sid.is_null() {
    return Err(WinSandboxError::NullPointer {
      func: "ConvertStringSidToSidW",
    });
  }
  Ok(OwnedSid::from_local_free(sid as _))
}

fn create_restricted_token_disable_sids(
  existing_token: HANDLE,
  disabled: &[&OwnedSid],
) -> Result<OwnedHandle> {
  let disabled: Vec<SID_AND_ATTRIBUTES> = disabled
    .iter()
    .map(|sid| SID_AND_ATTRIBUTES {
      Sid: sid.as_ptr(),
      Attributes: 0,
    })
    .collect();
  let disabled_ptr = if disabled.is_empty() {
    std::ptr::null()
  } else {
    disabled.as_ptr()
  };

  let mut restricted: HANDLE = std::ptr::null_mut();
  let ok = unsafe {
    CreateRestrictedToken(
      existing_token,
      DISABLE_MAX_PRIVILEGE,
      disabled.len() as u32,
      disabled_ptr,
      0,
      std::ptr::null(),
      0,
      std::ptr::null(),
      &mut restricted,
    )
  };
  if ok == 0 {
    return Err(WinSandboxError::last("CreateRestrictedToken"));
  }
  if restricted.is_null() {
    return Err(WinSandboxError::NullPointer {
      func: "CreateRestrictedToken",
    });
  }
  Ok(OwnedHandle::from_raw(restricted))
}

fn should_retry_disabled_sids(err: &WinSandboxError) -> bool {
  const ERROR_INVALID_SID: u32 = 1337;
  match err {
    WinSandboxError::Win32 { code, .. } => {
      *code == ERROR_INVALID_PARAMETER || *code == ERROR_INVALID_SID
    }
    _ => false,
  }
}

fn wide_from_str(value: &str) -> Vec<u16> {
  let mut wide: Vec<u16> = value.encode_utf16().collect();
  wide.push(0);
  wide
}

fn win32_bool(func: &'static str, value: i32) -> Result<()> {
  if value == 0 {
    Err(WinSandboxError::last(func))
  } else {
    Ok(())
  }
}
