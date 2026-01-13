use crate::{AppContainerProfile, OwnedHandle, OwnedSid, Result, WinSandboxError};

use std::ffi::{c_void, OsStr, OsString};
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use windows_sys::Win32::Foundation::{
  CloseHandle, GetHandleInformation, SetHandleInformation, ERROR_ACCESS_DENIED, FALSE,
  HANDLE_FLAG_INHERIT, INVALID_HANDLE_VALUE, TRUE,
};
use windows_sys::Win32::Security::{NO_INHERITANCE, SECURITY_CAPABILITIES};
use windows_sys::Win32::Storage::FileSystem::{FILE_GENERIC_EXECUTE, FILE_GENERIC_READ};
use windows_sys::Win32::System::Console::{
  GetStdHandle, STD_ERROR_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE,
};
use windows_sys::Win32::System::Threading::{
  CreateProcessW, DeleteProcThreadAttributeList, InitializeProcThreadAttributeList, ResumeThread,
  UpdateProcThreadAttribute, CREATE_SUSPENDED, EXTENDED_STARTUPINFO_PRESENT,
  LPPROC_THREAD_ATTRIBUTE_LIST, PROCESS_INFORMATION, STARTUPINFOEXW, STARTUPINFOW,
};

use windows_sys::Win32::Security::Authorization::{
  ConvertStringSidToSidW, GetNamedSecurityInfoW, SetEntriesInAclW, SetNamedSecurityInfoW,
  EXPLICIT_ACCESS_W, GRANT_ACCESS, NO_MULTIPLE_TRUSTEE, SE_FILE_OBJECT, TRUSTEE_IS_SID,
  TRUSTEE_IS_UNKNOWN, TRUSTEE_W,
};
use windows_sys::Win32::Security::ACL;

// `PROC_THREAD_ATTRIBUTE_*` values are stable ABI constants from winbase.h:
//   ProcThreadAttributeValue(Number, Thread, Input, Additive)
// Keep them as explicit values so the crate does not rely on a specific `windows-sys` version
// exporting the constants.
const PROC_THREAD_ATTRIBUTE_HANDLE_LIST: usize = 0x0002_0002;
const PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES: usize = 0x0002_0009;
// ProcThreadAttributeValue(15, FALSE, TRUE, FALSE) → 0x0002_000F.
const PROC_THREAD_ATTRIBUTE_ALL_APPLICATION_PACKAGES_POLICY: usize = 0x0002_000F;
// ProcThreadAttributeValue(7, FALSE, TRUE, FALSE) → 0x0002_0007.
const PROC_THREAD_ATTRIBUTE_MITIGATION_POLICY: usize = 0x0002_0007;

// Value for `PROC_THREAD_ATTRIBUTE_ALL_APPLICATION_PACKAGES_POLICY` (winbase.h).
const PROCESS_CREATION_ALL_APPLICATION_PACKAGES_POLICY_BLOCK: u32 = 1;

/// Sandbox configuration for spawning untrusted renderer processes.
///
/// Today this type is intentionally small: it only supports spawning a child process inside a
/// **no-capabilities AppContainer**. This is the strongest Windows sandbox mode available for our
/// renderer process model.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RendererSandbox {
  appcontainer_name: &'static str,
}

impl RendererSandbox {
  /// Creates a sandbox configuration that spawns the child in a no-capabilities AppContainer.
  ///
  /// This corresponds to the "renderer AppContainer" described in `docs/sandboxing.md`.
  pub fn appcontainer_no_capabilities() -> Self {
    Self {
      appcontainer_name: "FastRender.Renderer",
    }
  }

  /// Spawns `exe` with `args` under the configured sandbox.
  ///
  /// Note: this helper inherits the parent process environment by default; it does **not** perform
  /// environment sanitization.
  pub fn spawn(&self, exe: &Path, args: &[OsString]) -> Result<SandboxedChild> {
    spawn_appcontainer_no_capabilities(self.appcontainer_name, exe, args)
  }
}

/// A child process spawned inside the Windows renderer sandbox.
#[derive(Debug)]
pub struct SandboxedChild {
  pub process: OwnedHandle,
  pub pid: u32,
  /// Keep any relocated executable alive for the lifetime of the child handle.
  _temp_dir: Option<TempDir>,
}

fn spawn_appcontainer_no_capabilities(
  appcontainer_name: &str,
  exe: &Path,
  args: &[OsString],
) -> Result<SandboxedChild> {
  let profile = AppContainerProfile::ensure(
    appcontainer_name,
    "FastRender Renderer",
    "FastRender renderer AppContainer profile",
  )?;

  let mut capabilities = SECURITY_CAPABILITIES {
    AppContainerSid: profile.sid().as_ptr(),
    Capabilities: std::ptr::null_mut(),
    CapabilityCount: 0,
    Reserved: 0,
  };

  let mut handles = standard_handle_list();
  let _inherit_guard = HandleInheritGuard::new(&handles);
  let mut all_packages_policy_value = PROCESS_CREATION_ALL_APPLICATION_PACKAGES_POLICY_BLOCK;

  fn mitigation_policy_unsupported(err: &WinSandboxError) -> bool {
    const ERROR_INVALID_PARAMETER: u32 = 87;
    const ERROR_NOT_SUPPORTED: u32 = windows_sys::Win32::Foundation::ERROR_NOT_SUPPORTED;
    matches!(
      err,
      WinSandboxError::Win32 { code, .. }
        if *code == ERROR_INVALID_PARAMETER || *code == ERROR_NOT_SUPPORTED
    )
  }

  let mitigation_policy = crate::mitigations::renderer_mitigation_policy();
  let mut mitigation_policy_value = mitigation_policy;

  let base_attribute_count = 1 + u32::from(!handles.is_empty());
  let attribute_count_with_aap = base_attribute_count + 1;

  let mut init_attrs_base = |attrs: &mut AttributeList, include_aap: bool| -> Result<()> {
    attrs.update_raw(
      PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES,
      std::ptr::addr_of_mut!(capabilities).cast::<c_void>(),
      std::mem::size_of::<SECURITY_CAPABILITIES>(),
    )?;

    if include_aap {
      if let Err(err) = attrs.update_raw(
        PROC_THREAD_ATTRIBUTE_ALL_APPLICATION_PACKAGES_POLICY,
        std::ptr::addr_of_mut!(all_packages_policy_value).cast::<c_void>(),
        std::mem::size_of::<u32>(),
      ) {
        if mitigation_policy_unsupported(&err) {
          eprintln!(
            "warning: win-sandbox RendererSandbox: AAP hardening attribute unsupported ({err}); continuing without it"
          );
        } else {
          return Err(err);
        }
      }
    }

    if !handles.is_empty() {
      attrs.update_raw(
        PROC_THREAD_ATTRIBUTE_HANDLE_LIST,
        handles.as_mut_ptr().cast::<c_void>(),
        handles.len() * std::mem::size_of::<windows_sys::Win32::Foundation::HANDLE>(),
      )?;
    }
    Ok(())
  };

  let mut attrs_without_mitigations = AttributeList::new(attribute_count_with_aap)?;
  init_attrs_base(&mut attrs_without_mitigations, true)?;

  let mut attrs_without_aap = AttributeList::new(base_attribute_count)?;
  init_attrs_base(&mut attrs_without_aap, false)?;

  let mut attrs_with_mitigations: Option<AttributeList> = None;
  let mut attrs_with_mitigations_no_aap: Option<AttributeList> = None;

  if mitigation_policy_value != 0 {
    let mut attrs = AttributeList::new(attribute_count_with_aap + 1)?;
    init_attrs_base(&mut attrs, true)?;
    match attrs.update_raw(
      PROC_THREAD_ATTRIBUTE_MITIGATION_POLICY,
      std::ptr::addr_of_mut!(mitigation_policy_value).cast::<c_void>(),
      std::mem::size_of::<u64>(),
    ) {
      Ok(()) => attrs_with_mitigations = Some(attrs),
      Err(err) if mitigation_policy_unsupported(&err) => {
        eprintln!(
          "warning: win-sandbox RendererSandbox: mitigation policy attribute unsupported ({err}); continuing without mitigations"
        );
      }
      Err(err) => return Err(err),
    }

    let mut attrs = AttributeList::new(base_attribute_count + 1)?;
    init_attrs_base(&mut attrs, false)?;
    match attrs.update_raw(
      PROC_THREAD_ATTRIBUTE_MITIGATION_POLICY,
      std::ptr::addr_of_mut!(mitigation_policy_value).cast::<c_void>(),
      std::mem::size_of::<u64>(),
    ) {
      Ok(()) => attrs_with_mitigations_no_aap = Some(attrs),
      Err(err) if mitigation_policy_unsupported(&err) => {}
      Err(err) => return Err(err),
    }
  }

  let inherit = if handles.is_empty() { FALSE } else { TRUE };
  let flags = CREATE_SUSPENDED | EXTENDED_STARTUPINFO_PRESENT;

  let system32 = system32_dir();
  let system32_w = wide_null(system32.as_os_str());

  let create_process = |image: &Path, current_dir: Option<&[u16]>| -> Result<PROCESS_INFORMATION> {
    let application_name = wide_null(image.as_os_str());
    let current_dir_ptr = current_dir
      .map(|wide| wide.as_ptr())
      .unwrap_or(std::ptr::null());

    let create_process_with_attrs = |attr_list| -> Result<PROCESS_INFORMATION> {
      let mut cmdline = build_command_line(image, args);

      let mut startup: STARTUPINFOEXW = unsafe { std::mem::zeroed() };
      startup.StartupInfo.cb = std::mem::size_of::<STARTUPINFOEXW>() as u32;
      startup.lpAttributeList = attr_list;

      let mut pi: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };
      let ok = unsafe {
        CreateProcessW(
          application_name.as_ptr(),
          cmdline.as_mut_ptr(),
          std::ptr::null(),
          std::ptr::null(),
          inherit,
          flags,
          std::ptr::null(),
          current_dir_ptr,
          std::ptr::addr_of_mut!(startup).cast::<STARTUPINFOW>(),
          &mut pi,
        )
      };
      if ok == 0 {
        return Err(WinSandboxError::last("CreateProcessW"));
      }
      Ok(pi)
    };

    let mut attempts: Vec<(LPPROC_THREAD_ATTRIBUTE_LIST, &'static str)> = Vec::new();
    if let Some(attrs) = attrs_with_mitigations.as_ref() {
      attempts.push((attrs.list, "mitigations + AAP hardening"));
    }
    attempts.push((attrs_without_mitigations.list, "AAP hardening"));
    if let Some(attrs) = attrs_with_mitigations_no_aap.as_ref() {
      attempts.push((attrs.list, "mitigations (no AAP hardening)"));
    }
    attempts.push((attrs_without_aap.list, "no mitigations, no AAP hardening"));

    let mut last_optional_err: Option<WinSandboxError> = None;
    for (list, label) in attempts {
      match create_process_with_attrs(list) {
        Ok(pi) => return Ok(pi),
        Err(err) if mitigation_policy_unsupported(&err) => {
          eprintln!(
            "warning: win-sandbox RendererSandbox: CreateProcessW rejected startup attributes ({label}): {err}; retrying with weaker attribute set"
          );
          last_optional_err = Some(err);
          continue;
        }
        Err(err) => return Err(err),
      }
    }

    Err(last_optional_err.unwrap_or_else(|| WinSandboxError::from_code("CreateProcessW", 0)))
  };

  match create_process(exe, Some(&system32_w)) {
    Ok(pi) => return finish_spawn(pi, None),
    Err(err) => {
      if !matches!(err, WinSandboxError::Win32 { code, .. } if code == ERROR_ACCESS_DENIED) {
        return Err(err);
      }
    }
  }

  // Developer builds / CI checkouts often reside in directories without AppContainer ACL entries.
  // Remediate by copying the executable to a fresh temp directory and granting read+execute to the
  // derived AppContainer SID.
  let (temp_dir, relocated) = relocate_exe_for_appcontainer(exe, profile.sid().as_ptr())?;
  let current_dir_w = wide_null(temp_dir.path().as_os_str());
  let pi = create_process(&relocated, Some(&current_dir_w))?;
  finish_spawn(pi, Some(temp_dir))
}

fn finish_spawn(pi: PROCESS_INFORMATION, temp_dir: Option<TempDir>) -> Result<SandboxedChild> {
  if pi.hProcess.is_null() {
    unsafe {
      if !pi.hThread.is_null() {
        CloseHandle(pi.hThread);
      }
    }
    return Err(WinSandboxError::NullPointer {
      func: "CreateProcessW (hProcess)",
    });
  }

  // Assigning to a job object would normally happen here, but the win-sandbox crate keeps that
  // policy separate (see `Job`).

  unsafe {
    // Resume the main thread now that the process has been created.
    let rc = ResumeThread(pi.hThread);
    if rc == u32::MAX {
      let err = WinSandboxError::last("ResumeThread");
      CloseHandle(pi.hThread);
      CloseHandle(pi.hProcess);
      return Err(err);
    }
    CloseHandle(pi.hThread);
  }

  Ok(SandboxedChild {
    process: OwnedHandle::from_raw(pi.hProcess),
    pid: pi.dwProcessId,
    _temp_dir: temp_dir,
  })
}

// -----------------------------------------------------------------------------
// Relocation + ACL helpers
// -----------------------------------------------------------------------------

#[derive(Debug)]
struct TempDir {
  path: PathBuf,
}

impl TempDir {
  fn new(prefix: &str) -> std::io::Result<Self> {
    let base = std::env::temp_dir();
    let pid = std::process::id();
    for attempt in 0..512u32 {
      let candidate = base.join(format!("{prefix}{pid}-{attempt}"));
      match std::fs::create_dir(&candidate) {
        Ok(()) => return Ok(Self { path: candidate }),
        Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => continue,
        Err(err) => return Err(err),
      }
    }
    Err(std::io::Error::new(
      std::io::ErrorKind::AlreadyExists,
      "failed to allocate a unique temp directory",
    ))
  }

  fn path(&self) -> &Path {
    &self.path
  }
}

impl Drop for TempDir {
  fn drop(&mut self) {
    let _ = std::fs::remove_dir_all(&self.path);
  }
}

fn relocate_exe_for_appcontainer(
  exe: &Path,
  appcontainer_sid: windows_sys::Win32::Security::PSID,
) -> Result<(TempDir, PathBuf)> {
  let file_name = exe
    .file_name()
    .filter(|name| !name.is_empty())
    .unwrap_or_else(|| OsStr::new("renderer.exe"));

  let temp_dir =
    TempDir::new("fastrender-appcontainer-image-").map_err(|err| WinSandboxError::Win32 {
      func: "create temp dir",
      code: err.raw_os_error().unwrap_or(1) as u32,
      message: err.to_string(),
    })?;

  let dst = temp_dir.path().join(file_name);
  std::fs::copy(exe, &dst).map_err(|err| WinSandboxError::Win32 {
    func: "copy appcontainer image",
    code: err.raw_os_error().unwrap_or(1) as u32,
    message: err.to_string(),
  })?;

  // Grant access to the directory itself (traverse checks).
  let _ = grant_read_execute_acl(temp_dir.path(), appcontainer_sid);

  // Prefer granting to the specific AppContainer SID (narrowest). If that fails unexpectedly,
  // fall back to ALL APPLICATION PACKAGES.
  if let Err(err) = grant_read_execute_acl(&dst, appcontainer_sid) {
    let aap = all_application_packages_sid()?;
    let _ = grant_read_execute_acl(temp_dir.path(), aap.as_ptr());
    grant_read_execute_acl(&dst, aap.as_ptr()).map_err(|_| err)?;
  }

  Ok((temp_dir, dst))
}

fn all_application_packages_sid() -> Result<OwnedSid> {
  // ALL APPLICATION PACKAGES: S-1-15-2-1.
  let sid_string = wide_null(OsStr::new("S-1-15-2-1"));
  let mut sid: windows_sys::Win32::Security::PSID = std::ptr::null_mut();
  let ok = unsafe { ConvertStringSidToSidW(sid_string.as_ptr(), &mut sid) };
  if ok == 0 {
    return Err(WinSandboxError::last(
      "ConvertStringSidToSidW(ALL APPLICATION PACKAGES)",
    ));
  }
  if sid.is_null() {
    return Err(WinSandboxError::NullPointer {
      func: "ConvertStringSidToSidW(ALL APPLICATION PACKAGES)",
    });
  }
  Ok(OwnedSid::from_local_free(sid))
}

fn grant_read_execute_acl(path: &Path, sid: windows_sys::Win32::Security::PSID) -> Result<()> {
  let mut name = wide_null(path.as_os_str());

  let mut dacl: *mut ACL = std::ptr::null_mut();
  let mut sd: *mut c_void = std::ptr::null_mut();

  let status = unsafe {
    GetNamedSecurityInfoW(
      name.as_mut_ptr(),
      SE_FILE_OBJECT,
      windows_sys::Win32::Security::DACL_SECURITY_INFORMATION,
      std::ptr::null_mut(),
      std::ptr::null_mut(),
      &mut dacl,
      std::ptr::null_mut(),
      &mut sd,
    )
  };
  if status != 0 {
    return Err(WinSandboxError::from_code("GetNamedSecurityInfoW", status));
  }

  let mut ea: EXPLICIT_ACCESS_W = unsafe { std::mem::zeroed() };
  ea.grfAccessPermissions = FILE_GENERIC_READ | FILE_GENERIC_EXECUTE;
  ea.grfAccessMode = GRANT_ACCESS;
  // `NO_INHERITANCE` is the default (0). We intentionally do not grant inheritance.
  ea.grfInheritance = NO_INHERITANCE;
  ea.Trustee = TRUSTEE_W {
    pMultipleTrustee: std::ptr::null_mut(),
    MultipleTrusteeOperation: NO_MULTIPLE_TRUSTEE,
    TrusteeForm: TRUSTEE_IS_SID,
    TrusteeType: TRUSTEE_IS_UNKNOWN,
    ptstrName: sid.cast::<u16>(),
  };

  let mut new_dacl: *mut ACL = std::ptr::null_mut();
  let status = unsafe { SetEntriesInAclW(1, &mut ea, dacl, &mut new_dacl) };
  if status != 0 {
    unsafe {
      windows_sys::Win32::Foundation::LocalFree(sd as _);
    }
    return Err(WinSandboxError::from_code("SetEntriesInAclW", status));
  }

  let status = unsafe {
    SetNamedSecurityInfoW(
      name.as_mut_ptr(),
      SE_FILE_OBJECT,
      windows_sys::Win32::Security::DACL_SECURITY_INFORMATION,
      std::ptr::null_mut(),
      std::ptr::null_mut(),
      new_dacl,
      std::ptr::null_mut(),
    )
  };

  unsafe {
    windows_sys::Win32::Foundation::LocalFree(sd as _);
    windows_sys::Win32::Foundation::LocalFree(new_dacl as _);
  }

  if status != 0 {
    return Err(WinSandboxError::from_code("SetNamedSecurityInfoW", status));
  }
  Ok(())
}

// -----------------------------------------------------------------------------
// Process/attribute plumbing helpers
// -----------------------------------------------------------------------------

struct AttributeList {
  list: LPPROC_THREAD_ATTRIBUTE_LIST,
  // Use `usize` buffer to guarantee pointer alignment.
  _buffer: Vec<usize>,
}

impl AttributeList {
  fn new(attribute_count: u32) -> Result<Self> {
    let mut size: usize = 0;
    unsafe {
      InitializeProcThreadAttributeList(std::ptr::null_mut(), attribute_count, 0, &mut size);
    }
    if size == 0 {
      return Err(WinSandboxError::last(
        "InitializeProcThreadAttributeList (query size)",
      ));
    }

    let units = (size + std::mem::size_of::<usize>() - 1) / std::mem::size_of::<usize>();
    let mut buffer = vec![0usize; units];
    let list: LPPROC_THREAD_ATTRIBUTE_LIST = buffer.as_mut_ptr().cast();
    let ok = unsafe { InitializeProcThreadAttributeList(list, attribute_count, 0, &mut size) };
    if ok == 0 {
      return Err(WinSandboxError::last("InitializeProcThreadAttributeList"));
    }

    Ok(Self {
      list,
      _buffer: buffer,
    })
  }

  fn update_raw(&mut self, attribute: usize, value: *mut c_void, size: usize) -> Result<()> {
    let ok = unsafe {
      UpdateProcThreadAttribute(
        self.list,
        0,
        attribute,
        value,
        size,
        std::ptr::null_mut(),
        std::ptr::null_mut(),
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
      DeleteProcThreadAttributeList(self.list);
    }
    // `_buffer` is dropped automatically.
  }
}

struct HandleInheritGuard {
  saved: Vec<(windows_sys::Win32::Foundation::HANDLE, u32)>,
}

impl HandleInheritGuard {
  fn new(handles: &[windows_sys::Win32::Foundation::HANDLE]) -> Self {
    let mut saved = Vec::with_capacity(handles.len());
    for &handle in handles {
      if handle.is_null() || handle == INVALID_HANDLE_VALUE {
        continue;
      }
      let mut flags: u32 = 0;
      let ok = unsafe { GetHandleInformation(handle, &mut flags) };
      if ok == 0 {
        continue;
      }
      saved.push((handle, flags));
      unsafe {
        let _ = SetHandleInformation(handle, HANDLE_FLAG_INHERIT, HANDLE_FLAG_INHERIT);
      }
    }
    Self { saved }
  }
}

impl Drop for HandleInheritGuard {
  fn drop(&mut self) {
    for (handle, flags) in self.saved.drain(..) {
      let inherit = if (flags & HANDLE_FLAG_INHERIT) != 0 {
        HANDLE_FLAG_INHERIT
      } else {
        0
      };
      unsafe {
        let _ = SetHandleInformation(handle, HANDLE_FLAG_INHERIT, inherit);
      }
    }
  }
}

fn standard_handle_list() -> Vec<windows_sys::Win32::Foundation::HANDLE> {
  let mut handles = Vec::new();
  for h in unsafe {
    [
      GetStdHandle(STD_INPUT_HANDLE),
      GetStdHandle(STD_OUTPUT_HANDLE),
      GetStdHandle(STD_ERROR_HANDLE),
    ]
  } {
    if h.is_null() || h == INVALID_HANDLE_VALUE {
      continue;
    }
    if !handles.contains(&h) {
      handles.push(h);
    }
  }
  handles
}

fn system32_dir() -> PathBuf {
  let root = std::env::var_os("SystemRoot").unwrap_or_else(|| OsString::from("C:\\Windows"));
  PathBuf::from(root).join("System32")
}

fn wide_null(value: &OsStr) -> Vec<u16> {
  value.encode_wide().chain(Some(0)).collect()
}

fn build_command_line(exe: &Path, args: &[OsString]) -> Vec<u16> {
  let mut cmd: Vec<u16> = Vec::new();
  append_cmd_arg(&mut cmd, exe.as_os_str());
  for arg in args {
    append_cmd_arg(&mut cmd, arg.as_os_str());
  }
  cmd.push(0);
  cmd
}

fn append_cmd_arg(cmd: &mut Vec<u16>, arg: &OsStr) {
  if !cmd.is_empty() {
    cmd.push(' ' as u16);
  }

  let wide: Vec<u16> = arg.encode_wide().collect();
  let needs_quotes = wide.is_empty()
    || wide
      .iter()
      .any(|c| *c == ' ' as u16 || *c == '\t' as u16 || *c == '"' as u16);
  if !needs_quotes {
    cmd.extend_from_slice(&wide);
    return;
  }

  cmd.push('"' as u16);
  let mut backslashes: usize = 0;
  for ch in wide {
    if ch == '\\' as u16 {
      backslashes += 1;
      continue;
    }

    if ch == '"' as u16 {
      cmd.extend(std::iter::repeat('\\' as u16).take(backslashes * 2 + 1));
      cmd.push('"' as u16);
      backslashes = 0;
      continue;
    }

    cmd.extend(std::iter::repeat('\\' as u16).take(backslashes));
    backslashes = 0;
    cmd.push(ch);
  }
  cmd.extend(std::iter::repeat('\\' as u16).take(backslashes * 2));
  cmd.push('"' as u16);
}
