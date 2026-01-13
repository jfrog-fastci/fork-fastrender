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

use crate::spawn::{
  build_command_line, create_default_job, effective_mitigation_policy, finish_spawn, wide_from_os,
  AttributeList, SandboxRequest, SpawnConfig, PROC_THREAD_ATTRIBUTE_MITIGATION_POLICY,
};
use crate::{OwnedHandle, OwnedSid, Result, WinSandboxError};

use windows_sys::Win32::Foundation::ERROR_INVALID_PARAMETER;
use windows_sys::Win32::Foundation::{FALSE, HANDLE, TRUE};
use windows_sys::Win32::Security::Authorization::ConvertStringSidToSidW;
use windows_sys::Win32::Security::{
  CreateRestrictedToken, GetLengthSid, SetTokenInformation, TokenIntegrityLevel, DISABLE_MAX_PRIVILEGE,
  PSID, SID_AND_ATTRIBUTES, TOKEN_ADJUST_DEFAULT, TOKEN_ASSIGN_PRIMARY, TOKEN_DUPLICATE,
  TOKEN_INFORMATION_CLASS, TOKEN_MANDATORY_LABEL, TOKEN_QUERY,
};
use windows_sys::Win32::System::SystemServices::{SE_GROUP_INTEGRITY, SE_GROUP_INTEGRITY_ENABLED};
use windows_sys::Win32::System::Threading::{
  CreateProcessAsUserW, GetCurrentProcess, OpenProcessToken, CREATE_SUSPENDED,
  EXTENDED_STARTUPINFO_PRESENT, PROCESS_INFORMATION, PROC_THREAD_ATTRIBUTE_HANDLE_LIST, STARTUPINFOEXW,
  STARTUPINFOW,
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
    None => return Err(last_err.unwrap_or_else(|| WinSandboxError::from_code("CreateRestrictedToken", 0))),
  };

  set_low_integrity(restricted.as_raw())?;
  Ok(restricted)
}

/// Spawn a child process with the provided primary token via `CreateProcessAsUserW`.
///
/// The process is created suspended, placed into a Job Object, then resumed. This ensures job
/// constraints (kill-on-close, active-process cap) are applied before the child runs arbitrary code.
pub fn spawn_with_token(cfg: &SpawnConfig, token: &OwnedHandle) -> Result<crate::ChildProcess> {
  let job = create_default_job()?;
  let application_name = wide_from_os(cfg.exe.as_os_str());
  // If `lpCurrentDirectory` is NULL, Windows inherits the parent's current directory. For low
  // integrity/restricted tokens that directory may be inaccessible (e.g. a dev checkout that is not
  // readable at Low IL), causing `CreateProcessAsUserW` to fail with `ERROR_ACCESS_DENIED`.
  //
  // Using the executable's parent directory is a best-effort choice: if the executable is
  // loadable, the directory is generally traversable too.
  let current_dir_wide = cfg.exe.parent().map(|p| wide_from_os(p.as_os_str()));
  let current_dir_ptr = current_dir_wide
    .as_ref()
    .map(|wide| wide.as_ptr())
    .unwrap_or(std::ptr::null());
  let mut cmdline = build_command_line(&cfg.exe, &cfg.args);

  let mut pi: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };

  let mitigation_policy = effective_mitigation_policy(cfg.mitigation_policy);
  let mut mitigation_policy_value = mitigation_policy;
  let mut handles = cfg.inherit_handles.clone();

  let attribute_count = u32::from(!handles.is_empty()) + u32::from(mitigation_policy != 0);

  if attribute_count == 0 {
    let mut startup: STARTUPINFOW = unsafe { std::mem::zeroed() };
    startup.cb = std::mem::size_of::<STARTUPINFOW>() as u32;
    let ok = unsafe {
      CreateProcessAsUserW(
        token.as_raw(),
        application_name.as_ptr(),
        cmdline.as_mut_ptr(),
        std::ptr::null(),
        std::ptr::null(),
        FALSE,
        CREATE_SUSPENDED,
        std::ptr::null(),
        current_dir_ptr,
        &startup,
        &mut pi,
      )
    };
    win32_bool("CreateProcessAsUserW", ok)?;
  } else {
    let mut attrs = AttributeList::new(attribute_count)?;
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

    let mut startup: STARTUPINFOEXW = unsafe { std::mem::zeroed() };
    startup.StartupInfo.cb = std::mem::size_of::<STARTUPINFOEXW>() as u32;
    startup.lpAttributeList = attrs.list;
    let flags = CREATE_SUSPENDED | EXTENDED_STARTUPINFO_PRESENT;
    let ok = unsafe {
      CreateProcessAsUserW(
        token.as_raw(),
        application_name.as_ptr(),
        cmdline.as_mut_ptr(),
        std::ptr::null(),
        std::ptr::null(),
        if handles.is_empty() { FALSE } else { TRUE },
        flags,
        std::ptr::null(),
        current_dir_ptr,
        std::ptr::addr_of_mut!(startup).cast::<STARTUPINFOW>(),
        &mut pi,
      )
    };
    win32_bool("CreateProcessAsUserW", ok)?;
  }

  finish_spawn(job, pi, SandboxRequest::RestrictedToken)
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
    WinSandboxError::Win32 { code, .. } => *code == ERROR_INVALID_PARAMETER || *code == ERROR_INVALID_SID,
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
