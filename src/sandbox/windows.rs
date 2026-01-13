//! Windows renderer sandboxing primitives.
//!
//! This is best-effort and intended for a future multiprocess browser architecture where
//! renderer processes run with substantially reduced OS capabilities.

use std::ffi::{OsStr, OsString};
use std::io;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle, RawHandle};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use tempfile::TempDir;

pub mod appcontainer;

/// Debug escape hatch: disable the Windows renderer sandbox.
///
/// This is intentionally Windows-only (the variable is ignored on other platforms).
const ENV_DISABLE_RENDERER_SANDBOX: &str = "FASTR_DISABLE_RENDERER_SANDBOX";

/// Legacy/alternative spelling for disabling the Windows renderer sandbox.
///
/// Accepted values:
/// - `off`, `0`, `false`, `no` (case-insensitive) => disable sandboxing
/// - any other non-empty value => leave sandboxing enabled (default)
const ENV_WINDOWS_RENDERER_SANDBOX: &str = "FASTR_WINDOWS_RENDERER_SANDBOX";

/// Enable verbose sandbox logging (primarily for Windows AppContainer spawn debugging).
const ENV_LOG_SANDBOX: &str = "FASTR_LOG_SANDBOX";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WindowsRendererSandboxLevel {
  /// Primary mode: AppContainer with zero capabilities.
  AppContainer,
  /// Fallback mode: restricted token + low integrity.
  RestrictedToken,
}

/// Returns the sandbox level the Windows renderer should attempt to use.
///
/// - `None` means "spawn unsandboxed" (debug escape hatch).
/// - `Some(..)` indicates the preferred sandbox mode; callers are expected to apply
///   fallbacks if a stronger sandbox is unavailable.
pub(crate) fn requested_renderer_sandbox_level() -> Option<WindowsRendererSandboxLevel> {
  if renderer_sandbox_disabled_via_env() {
    log_sandbox_disabled_once();
    return None;
  }

  // Preferred sandbox. If AppContainer is unavailable (e.g. older Windows versions without the
  // relevant userenv.dll exports), fall back to restricted-token mode.
  match appcontainer::appcontainer_apis() {
    Ok(_) => Some(WindowsRendererSandboxLevel::AppContainer),
    Err(err) => {
      log_appcontainer_unavailable_once(err);
      Some(WindowsRendererSandboxLevel::RestrictedToken)
    }
  }
}

fn renderer_sandbox_disabled_via_env() -> bool {
  if env_var_truthy(std::env::var_os(ENV_DISABLE_RENDERER_SANDBOX).as_deref()) {
    return true;
  }

  let Some(raw) = std::env::var_os(ENV_WINDOWS_RENDERER_SANDBOX) else {
    return false;
  };
  let raw = raw.to_string_lossy();
  let trimmed = raw.trim();
  if trimmed.is_empty() {
    return false;
  }
  matches!(
    trimmed.to_ascii_lowercase().as_str(),
    "0" | "false" | "no" | "off"
  )
}

fn env_var_truthy(raw: Option<&OsStr>) -> bool {
  let Some(raw) = raw else {
    return false;
  };
  if raw.is_empty() {
    return false;
  }
  let raw = raw.to_string_lossy();
  let trimmed = raw.trim();
  if trimmed.is_empty() {
    return false;
  }
  !matches!(
    trimmed.to_ascii_lowercase().as_str(),
    "0" | "false" | "no" | "off"
  )
}

fn log_sandbox_disabled_once() {
  static LOGGED: OnceLock<()> = OnceLock::new();
  LOGGED.get_or_init(|| {
    eprintln!(
      "warning: Windows renderer sandbox is DISABLED (debug escape hatch). \
Set {ENV_DISABLE_RENDERER_SANDBOX}=0/1 or {ENV_WINDOWS_RENDERER_SANDBOX}=off to control this."
    );
  });
}

fn log_sandbox_debug(msg: &str) {
  if cfg!(debug_assertions) || env_var_truthy(std::env::var_os(ENV_LOG_SANDBOX).as_deref()) {
    eprintln!("{msg}");
  }
}

fn log_appcontainer_unavailable_once(err: &appcontainer::AppContainerApiLoadError) {
  static LOGGED: OnceLock<()> = OnceLock::new();
  LOGGED.get_or_init(|| {
    eprintln!(
      "warning: Windows AppContainer sandbox is unavailable ({err}); falling back to restricted-token sandboxing"
    );
  });
}

use windows_sys::Win32::Foundation::{
  BOOL, ERROR_ACCESS_DENIED, ERROR_ALREADY_EXISTS, ERROR_SUCCESS, FALSE, HANDLE, TRUE,
};
use windows_sys::Win32::Security::Authorization::{
  GetNamedSecurityInfoW, SetEntriesInAclW, SetNamedSecurityInfoW, EXPLICIT_ACCESS_W, GRANT_ACCESS,
  NO_MULTIPLE_TRUSTEE, NO_INHERITANCE, SE_FILE_OBJECT, TRUSTEE_IS_SID, TRUSTEE_IS_UNKNOWN,
  TRUSTEE_W,
};
use windows_sys::Win32::Security::{
  ConvertStringSidToSidW, CreateRestrictedToken, FreeSid, GetLengthSid, OpenProcessToken,
  SetTokenInformation, TokenIntegrityLevel, DISABLE_MAX_PRIVILEGE, SECURITY_CAPABILITIES,
  SE_GROUP_INTEGRITY, SE_GROUP_INTEGRITY_ENABLED, TOKEN_ADJUST_DEFAULT, TOKEN_ASSIGN_PRIMARY,
  TOKEN_DUPLICATE, TOKEN_INFORMATION_CLASS, TOKEN_MANDATORY_LABEL, TOKEN_QUERY,
};
use windows_sys::Win32::Storage::FileSystem::{FILE_GENERIC_EXECUTE, FILE_GENERIC_READ};
use windows_sys::Win32::System::JobObjects::{
  AssignProcessToJobObject, CreateJobObjectW, JobObjectBasicUIRestrictions,
  JobObjectExtendedLimitInformation, SetInformationJobObject, JOBOBJECT_BASIC_UI_RESTRICTIONS,
  JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOB_OBJECT_LIMIT_ACTIVE_PROCESS,
  JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE, JOB_OBJECT_UILIMIT_DESKTOP,
  JOB_OBJECT_UILIMIT_DISPLAYSETTINGS, JOB_OBJECT_UILIMIT_EXITWINDOWS,
  JOB_OBJECT_UILIMIT_GLOBALATOMS, JOB_OBJECT_UILIMIT_HANDLES, JOB_OBJECT_UILIMIT_READCLIPBOARD,
  JOB_OBJECT_UILIMIT_SYSTEMPARAMETERS, JOB_OBJECT_UILIMIT_WRITECLIPBOARD,
};
use windows_sys::Win32::System::Memory::{GetProcessHeap, HeapAlloc, HeapFree, LocalFree};
use windows_sys::Win32::System::Threading::{
  CreateProcessAsUserW, CreateProcessW, DeleteProcThreadAttributeList, GetCurrentProcess,
  InitializeProcThreadAttributeList, ResumeThread, TerminateProcess, UpdateProcThreadAttribute,
  CREATE_SUSPENDED, EXTENDED_STARTUPINFO_PRESENT, PROCESS_INFORMATION,
  PROC_THREAD_ATTRIBUTE_HANDLE_LIST, PROC_THREAD_ATTRIBUTE_LIST,
  PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES, STARTUPINFOEXW, STARTUPINFOW,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WindowsSandboxLevel {
  AppContainer,
  RestrictedToken,
  None,
}

#[derive(Debug)]
pub struct SandboxedChild {
  pub process: OwnedHandle,
  pub pid: u32,
  pub job: JobObject,
  pub level: WindowsSandboxLevel,
  // Keep any relocated AppContainer executable alive for the lifetime of the child handle.
  _temp_dir: Option<TempDir>,
}

#[derive(Debug)]
pub struct JobObject {
  handle: OwnedHandle,
}

impl JobObject {
  pub fn new() -> io::Result<Self> {
    let handle = unsafe { CreateJobObjectW(std::ptr::null_mut(), std::ptr::null()) };
    if handle == 0 {
      return Err(io::Error::last_os_error());
    }
    let owned = unsafe { OwnedHandle::from_raw_handle(handle as RawHandle) };
    let job = Self { handle: owned };
    job.apply_limits()?;
    Ok(job)
  }

  fn raw(&self) -> HANDLE {
    self.handle.as_raw_handle() as HANDLE
  }

  fn apply_limits(&self) -> io::Result<()> {
    let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
    limits.BasicLimitInformation.LimitFlags =
      JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE | JOB_OBJECT_LIMIT_ACTIVE_PROCESS;
    limits.BasicLimitInformation.ActiveProcessLimit = 1;
    win32_bool(unsafe {
      SetInformationJobObject(
        self.raw(),
        JobObjectExtendedLimitInformation,
        std::ptr::addr_of_mut!(limits).cast(),
        std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
      )
    })?;

    // Optional UI restrictions: best-effort, ignored on failure.
    let mut ui: JOBOBJECT_BASIC_UI_RESTRICTIONS = unsafe { std::mem::zeroed() };
    ui.UIRestrictionsClass = JOB_OBJECT_UILIMIT_HANDLES
      | JOB_OBJECT_UILIMIT_READCLIPBOARD
      | JOB_OBJECT_UILIMIT_WRITECLIPBOARD
      | JOB_OBJECT_UILIMIT_SYSTEMPARAMETERS
      | JOB_OBJECT_UILIMIT_DISPLAYSETTINGS
      | JOB_OBJECT_UILIMIT_GLOBALATOMS
      | JOB_OBJECT_UILIMIT_EXITWINDOWS
      | JOB_OBJECT_UILIMIT_DESKTOP;
    let _ = win32_bool(unsafe {
      SetInformationJobObject(
        self.raw(),
        JobObjectBasicUIRestrictions,
        std::ptr::addr_of_mut!(ui).cast(),
        std::mem::size_of::<JOBOBJECT_BASIC_UI_RESTRICTIONS>() as u32,
      )
    });
    Ok(())
  }

  pub fn assign_process(&self, process: HANDLE) -> io::Result<()> {
    win32_bool(unsafe { AssignProcessToJobObject(self.raw(), process) })
  }
}

struct AttributeList {
  heap: HANDLE,
  alloc: *mut std::ffi::c_void,
  list: *mut PROC_THREAD_ATTRIBUTE_LIST,
  initialized: bool,
}

impl AttributeList {
  fn new(attribute_count: u32) -> io::Result<Self> {
    let mut size: usize = 0;
    // Query required size.
    unsafe {
      InitializeProcThreadAttributeList(std::ptr::null_mut(), attribute_count, 0, &mut size);
    }

    let heap = unsafe { GetProcessHeap() };
    if heap == 0 {
      return Err(io::Error::last_os_error());
    }
    let alloc = unsafe { HeapAlloc(heap, 0, size) };
    if alloc.is_null() {
      return Err(io::Error::last_os_error());
    }
    let list = alloc.cast::<PROC_THREAD_ATTRIBUTE_LIST>();
    let ok = unsafe { InitializeProcThreadAttributeList(list, attribute_count, 0, &mut size) };
    if ok == 0 {
      let err = io::Error::last_os_error();
      unsafe {
        HeapFree(heap, 0, alloc);
      }
      return Err(err);
    }
    Ok(Self {
      heap,
      alloc,
      list,
      initialized: true,
    })
  }

  fn update_raw(
    &mut self,
    attr: usize,
    value: *mut std::ffi::c_void,
    size: usize,
  ) -> io::Result<()> {
    win32_bool(unsafe {
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
    unsafe {
      if self.initialized {
        DeleteProcThreadAttributeList(self.list);
      }
      if !self.alloc.is_null() && self.heap != 0 {
        HeapFree(self.heap, 0, self.alloc);
      }
    }
  }
}

struct AppContainerSid {
  sid: *mut std::ffi::c_void,
}

impl AppContainerSid {
  fn new(name: &[u16]) -> io::Result<Self> {
    let apis = appcontainer::appcontainer_apis()
      .map_err(|err| io::Error::new(io::ErrorKind::Unsupported, err.to_string()))?;

    // Best-effort profile creation: ignore failures other than ensuring the profile exists.
    let display = wide_from_str("FastRender renderer");
    let description = wide_from_str("FastRender renderer sandbox profile");
    let mut created_sid: *mut std::ffi::c_void = std::ptr::null_mut();
    let hr = unsafe {
      (apis.create_app_container_profile)(
        name.as_ptr(),
        display.as_ptr(),
        description.as_ptr(),
        std::ptr::null(),
        0,
        &mut created_sid,
      )
    };
    if hr == 0 && !created_sid.is_null() {
      unsafe {
        FreeSid(created_sid);
      }
    }
    let hr_already_exists = hresult_from_win32(ERROR_ALREADY_EXISTS);
    if hr != 0 && hr != hr_already_exists {
      // Ignore and fall through to SID derivation (profile might already exist).
    }

    let mut sid: *mut std::ffi::c_void = std::ptr::null_mut();
    let hr =
      unsafe { (apis.derive_app_container_sid_from_app_container_name)(name.as_ptr(), &mut sid) };
    if hr < 0 {
      return Err(io_error_from_hresult(hr));
    }
    if sid.is_null() {
      return Err(io::Error::new(
        io::ErrorKind::Other,
        "DeriveAppContainerSidFromAppContainerName returned null SID",
      ));
    }
    Ok(Self { sid })
  }
}

impl Drop for AppContainerSid {
  fn drop(&mut self) {
    unsafe {
      if !self.sid.is_null() {
        FreeSid(self.sid);
      }
    }
  }
}

struct LocalAllocSid {
  sid: *mut std::ffi::c_void,
}

impl Drop for LocalAllocSid {
  fn drop(&mut self) {
    unsafe {
      if !self.sid.is_null() {
        LocalFree(self.sid as isize);
      }
    }
  }
}

pub fn spawn_sandboxed(
  exe: &Path,
  args: &[OsString],
  inherit_handles: &[RawHandle],
) -> io::Result<SandboxedChild> {
  let Some(requested) = requested_renderer_sandbox_level() else {
    return spawn_unsandboxed(exe, args, inherit_handles);
  };

  match requested {
    WindowsRendererSandboxLevel::AppContainer => match spawn_appcontainer(exe, args, inherit_handles)
    {
      Ok(child) => Ok(child),
      Err(appcontainer_err) => {
        eprintln!(
          "warning: Windows AppContainer sandbox failed ({appcontainer_err}); falling back to restricted-token mode"
        );
        match spawn_restricted_token(exe, args, inherit_handles) {
          Ok(child) => Ok(child),
          Err(restricted_err) => match spawn_unsandboxed(exe, args, inherit_handles) {
            Ok(child) => Ok(child),
            Err(unsandboxed_err) => Err(io::Error::new(
              unsandboxed_err.kind(),
              format!(
                "failed to spawn child process (appcontainer={appcontainer_err}, restricted_token={restricted_err}, unsandboxed={unsandboxed_err})"
              ),
            )),
          },
        }
      }
    },
    WindowsRendererSandboxLevel::RestrictedToken => match spawn_restricted_token(exe, args, inherit_handles)
    {
      Ok(child) => Ok(child),
      Err(restricted_err) => match spawn_unsandboxed(exe, args, inherit_handles) {
        Ok(child) => Ok(child),
        Err(unsandboxed_err) => Err(io::Error::new(
          unsandboxed_err.kind(),
          format!(
            "failed to spawn child process (restricted_token={restricted_err}, unsandboxed={unsandboxed_err})"
          ),
        )),
      },
    },
  }
}

fn spawn_appcontainer(
  exe: &Path,
  args: &[OsString],
  inherit_handles: &[RawHandle],
) -> io::Result<SandboxedChild> {
  let job = JobObject::new()?;
  let name = wide_from_str("FastRender.Renderer");
  let sid = AppContainerSid::new(&name)?;

  let mut capabilities = SECURITY_CAPABILITIES {
    AppContainerSid: sid.sid,
    Capabilities: std::ptr::null_mut(),
    CapabilityCount: 0,
    Reserved: 0,
  };

  let handles: Vec<HANDLE> = inherit_handles
    .iter()
    .copied()
    .map(|h| h as HANDLE)
    .collect();
  let mut handles = handles;

  let attribute_count = 1 + u32::from(!handles.is_empty());
  let mut attrs = AttributeList::new(attribute_count)?;
  attrs.update_raw(
    PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES,
    std::ptr::addr_of_mut!(capabilities).cast(),
    std::mem::size_of::<SECURITY_CAPABILITIES>(),
  )?;
  if !handles.is_empty() {
    attrs.update_raw(
      PROC_THREAD_ATTRIBUTE_HANDLE_LIST,
      handles.as_mut_ptr().cast(),
      handles.len() * std::mem::size_of::<HANDLE>(),
    )?;
  }

  let inherit = if handles.is_empty() { FALSE } else { TRUE };
  let flags = CREATE_SUSPENDED | EXTENDED_STARTUPINFO_PRESENT;

  let mut create_process = |image: &Path| -> io::Result<PROCESS_INFORMATION> {
    let application_name = wide_from_os(image.as_os_str());
    let mut cmdline = build_command_line(image, args);

    let mut startup: STARTUPINFOEXW = unsafe { std::mem::zeroed() };
    startup.StartupInfo.cb = std::mem::size_of::<STARTUPINFOEXW>() as u32;
    startup.lpAttributeList = attrs.list;

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
        std::ptr::null(),
        std::ptr::addr_of_mut!(startup).cast::<STARTUPINFOW>(),
        &mut pi,
      )
    };
    if ok == 0 {
      return Err(io::Error::last_os_error());
    }
    Ok(pi)
  };

  match create_process(exe) {
    Ok(pi) => return finish_spawn(job, pi, WindowsSandboxLevel::AppContainer, None),
    Err(err) => {
      if err.raw_os_error() != Some(ERROR_ACCESS_DENIED as i32) {
        return Err(err);
      }
    }
  }

  // AppContainer can fail to execute un-packaged dev/test binaries because the directory does not
  // grant read/execute to the sandbox token. Remediate by copying the image to a fresh temp dir and
  // granting access to the derived AppContainer SID, then retrying.
  let (temp_dir, relocated) = relocate_exe_for_appcontainer(exe, sid.sid)?;
  log_sandbox_debug(&format!(
    "windows sandbox: AppContainer CreateProcessW returned ERROR_ACCESS_DENIED for {}; copied image to {} and retrying",
    exe.display(),
    relocated.display()
  ));

  match create_process(&relocated) {
    Ok(pi) => finish_spawn(
      job,
      pi,
      WindowsSandboxLevel::AppContainer,
      Some(temp_dir),
    ),
    Err(err) => {
      eprintln!(
        "warning: Windows AppContainer retry after relocation failed ({err}); falling back to restricted-token mode"
      );
      Err(err)
    }
  }
}

fn relocate_exe_for_appcontainer(
  exe: &Path,
  appcontainer_sid: *mut std::ffi::c_void,
) -> io::Result<(TempDir, PathBuf)> {
  let file_name = exe
    .file_name()
    .filter(|name| !name.is_empty())
    .unwrap_or_else(|| OsStr::new("fastrender-renderer.exe"));

  let temp_dir = tempfile::Builder::new()
    .prefix("fastrender-appcontainer-image-")
    .tempdir()?;

  let dst = temp_dir.path().join(file_name);
  std::fs::copy(exe, &dst)?;

  // Best-effort: grant access to the directory itself as well (helps on stricter traverse checks).
  if let Err(err) = grant_read_execute_acl(temp_dir.path(), appcontainer_sid) {
    log_sandbox_debug(&format!(
      "windows sandbox: failed to grant AppContainer directory ACL for {} ({err}); continuing with file ACL",
      temp_dir.path().display()
    ));
  }

  // Prefer granting to the specific AppContainer SID (narrowest). If that fails unexpectedly,
  // fall back to ALL APPLICATION PACKAGES.
  if let Err(err) = grant_read_execute_acl(&dst, appcontainer_sid) {
    log_sandbox_debug(&format!(
      "windows sandbox: failed to grant copied image ACL to AppContainer SID ({err}); falling back to ALL APPLICATION PACKAGES"
    ));
    let aap = all_application_packages_sid()?;
    if let Err(err) = grant_read_execute_acl(temp_dir.path(), aap.sid) {
      log_sandbox_debug(&format!(
        "windows sandbox: failed to grant directory ACL to ALL APPLICATION PACKAGES for {} ({err})",
        temp_dir.path().display()
      ));
    }
    grant_read_execute_acl(&dst, aap.sid)?;
  }

  Ok((temp_dir, dst))
}

fn all_application_packages_sid() -> io::Result<LocalAllocSid> {
  // ALL APPLICATION PACKAGES: S-1-15-2-1.
  let sid_string = wide_from_str("S-1-15-2-1");
  let mut sid: *mut std::ffi::c_void = std::ptr::null_mut();
  win32_bool(unsafe { ConvertStringSidToSidW(sid_string.as_ptr(), &mut sid) })?;
  if sid.is_null() {
    return Err(io::Error::new(
      io::ErrorKind::Other,
      "ConvertStringSidToSidW returned null SID for ALL APPLICATION PACKAGES",
    ));
  }
  Ok(LocalAllocSid { sid })
}

fn grant_read_execute_acl(path: &Path, sid: *mut std::ffi::c_void) -> io::Result<()> {
  let mut name = wide_from_os(path.as_os_str());

  let mut dacl: *mut windows_sys::Win32::Security::ACL = std::ptr::null_mut();
  let mut sd: *mut std::ffi::c_void = std::ptr::null_mut();

  // SAFETY: FFI call; output pointers are writable.
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
  if status != ERROR_SUCCESS {
    return Err(io::Error::from_raw_os_error(status as i32));
  }

  let mut ea: EXPLICIT_ACCESS_W = unsafe { std::mem::zeroed() };
  ea.grfAccessPermissions = FILE_GENERIC_READ | FILE_GENERIC_EXECUTE;
  ea.grfAccessMode = GRANT_ACCESS;
  ea.grfInheritance = NO_INHERITANCE;
  ea.Trustee = TRUSTEE_W {
    pMultipleTrustee: std::ptr::null_mut(),
    MultipleTrusteeOperation: NO_MULTIPLE_TRUSTEE,
    TrusteeForm: TRUSTEE_IS_SID,
    TrusteeType: TRUSTEE_IS_UNKNOWN,
    ptstrName: sid as *mut _,
  };

  let mut new_dacl: *mut windows_sys::Win32::Security::ACL = std::ptr::null_mut();
  let status = unsafe { SetEntriesInAclW(1, &mut ea, dacl, &mut new_dacl) };
  if status != ERROR_SUCCESS {
    unsafe { LocalFree(sd as isize) };
    return Err(io::Error::from_raw_os_error(status as i32));
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
    LocalFree(sd as isize);
    LocalFree(new_dacl as isize);
  }

  if status != ERROR_SUCCESS {
    return Err(io::Error::from_raw_os_error(status as i32));
  }
  Ok(())
}

fn spawn_restricted_token(
  exe: &Path,
  args: &[OsString],
  inherit_handles: &[RawHandle],
) -> io::Result<SandboxedChild> {
  let job = JobObject::new()?;

  let mut token: HANDLE = 0;
  win32_bool(unsafe {
    OpenProcessToken(
      GetCurrentProcess(),
      TOKEN_DUPLICATE | TOKEN_QUERY | TOKEN_ASSIGN_PRIMARY | TOKEN_ADJUST_DEFAULT,
      &mut token,
    )
  })?;
  if token == 0 {
    return Err(io::Error::new(
      io::ErrorKind::Other,
      "OpenProcessToken returned null token handle",
    ));
  }
  let token = unsafe { OwnedHandle::from_raw_handle(token as RawHandle) };

  let mut restricted: HANDLE = 0;
  win32_bool(unsafe {
    CreateRestrictedToken(
      token.as_raw_handle() as HANDLE,
      DISABLE_MAX_PRIVILEGE,
      0,
      std::ptr::null(),
      0,
      std::ptr::null(),
      0,
      std::ptr::null(),
      &mut restricted,
    )
  })?;
  if restricted == 0 {
    return Err(io::Error::new(
      io::ErrorKind::Other,
      "CreateRestrictedToken returned null token handle",
    ));
  }
  let restricted = unsafe { OwnedHandle::from_raw_handle(restricted as RawHandle) };
  set_low_integrity(restricted.as_raw_handle() as HANDLE)?;

  let application_name = wide_from_os(exe.as_os_str());
  let mut cmdline = build_command_line(exe, args);

  let handles: Vec<HANDLE> = inherit_handles
    .iter()
    .copied()
    .map(|h| h as HANDLE)
    .collect();
  let mut handles = handles;
  let mut pi: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };

  if handles.is_empty() {
    let mut startup: STARTUPINFOW = unsafe { std::mem::zeroed() };
    startup.cb = std::mem::size_of::<STARTUPINFOW>() as u32;
    let ok = unsafe {
      CreateProcessAsUserW(
        restricted.as_raw_handle() as HANDLE,
        application_name.as_ptr(),
        cmdline.as_mut_ptr(),
        std::ptr::null(),
        std::ptr::null(),
        FALSE,
        CREATE_SUSPENDED,
        std::ptr::null(),
        std::ptr::null(),
        &startup,
        &mut pi,
      )
    };
    if ok == 0 {
      return Err(io::Error::last_os_error());
    }
  } else {
    let mut attrs = AttributeList::new(1)?;
    attrs.update_raw(
      PROC_THREAD_ATTRIBUTE_HANDLE_LIST,
      handles.as_mut_ptr().cast(),
      handles.len() * std::mem::size_of::<HANDLE>(),
    )?;

    let mut startup: STARTUPINFOEXW = unsafe { std::mem::zeroed() };
    startup.StartupInfo.cb = std::mem::size_of::<STARTUPINFOEXW>() as u32;
    startup.lpAttributeList = attrs.list;
    let flags = CREATE_SUSPENDED | EXTENDED_STARTUPINFO_PRESENT;
    let ok = unsafe {
      CreateProcessAsUserW(
        restricted.as_raw_handle() as HANDLE,
        application_name.as_ptr(),
        cmdline.as_mut_ptr(),
        std::ptr::null(),
        std::ptr::null(),
        TRUE,
        flags,
        std::ptr::null(),
        std::ptr::null(),
        std::ptr::addr_of_mut!(startup).cast::<STARTUPINFOW>(),
        &mut pi,
      )
    };
    if ok == 0 {
      return Err(io::Error::last_os_error());
    }
  }

  finish_spawn(job, pi, WindowsSandboxLevel::RestrictedToken, None)
}

fn spawn_unsandboxed(
  exe: &Path,
  args: &[OsString],
  inherit_handles: &[RawHandle],
) -> io::Result<SandboxedChild> {
  let job = JobObject::new()?;
  let application_name = wide_from_os(exe.as_os_str());
  let mut cmdline = build_command_line(exe, args);

  let handles: Vec<HANDLE> = inherit_handles
    .iter()
    .copied()
    .map(|h| h as HANDLE)
    .collect();
  let mut handles = handles;
  let mut pi: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };

  if handles.is_empty() {
    let mut startup: STARTUPINFOW = unsafe { std::mem::zeroed() };
    startup.cb = std::mem::size_of::<STARTUPINFOW>() as u32;
    let ok = unsafe {
      CreateProcessW(
        application_name.as_ptr(),
        cmdline.as_mut_ptr(),
        std::ptr::null(),
        std::ptr::null(),
        FALSE,
        CREATE_SUSPENDED,
        std::ptr::null(),
        std::ptr::null(),
        &startup,
        &mut pi,
      )
    };
    if ok == 0 {
      return Err(io::Error::last_os_error());
    }
  } else {
    let mut attrs = AttributeList::new(1)?;
    attrs.update_raw(
      PROC_THREAD_ATTRIBUTE_HANDLE_LIST,
      handles.as_mut_ptr().cast(),
      handles.len() * std::mem::size_of::<HANDLE>(),
    )?;

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
        TRUE,
        flags,
        std::ptr::null(),
        std::ptr::null(),
        std::ptr::addr_of_mut!(startup).cast::<STARTUPINFOW>(),
        &mut pi,
      )
    };
    if ok == 0 {
      return Err(io::Error::last_os_error());
    }
  }

  finish_spawn(job, pi, WindowsSandboxLevel::None, None)
}

fn finish_spawn(
  job: JobObject,
  pi: PROCESS_INFORMATION,
  level: WindowsSandboxLevel,
  temp_dir: Option<TempDir>,
) -> io::Result<SandboxedChild> {
  if pi.hProcess == 0 || pi.hThread == 0 {
    return Err(io::Error::new(
      io::ErrorKind::Other,
      "process creation returned null handles",
    ));
  }
  let pid = pi.dwProcessId;

  let process = unsafe { OwnedHandle::from_raw_handle(pi.hProcess as RawHandle) };
  let thread = unsafe { OwnedHandle::from_raw_handle(pi.hThread as RawHandle) };
  let h_process = process.as_raw_handle() as HANDLE;

  if let Err(err) = job.assign_process(h_process) {
    let _ = unsafe { TerminateProcess(h_process, 1) };
    return Err(err);
  }

  let resume_rc = unsafe { ResumeThread(thread.as_raw_handle() as HANDLE) };
  if resume_rc == u32::MAX {
    let err = io::Error::last_os_error();
    let _ = unsafe { TerminateProcess(h_process, 1) };
    return Err(err);
  }
  drop(thread);

  Ok(SandboxedChild {
    process,
    pid,
    job,
    level,
    _temp_dir: temp_dir,
  })
}

fn set_low_integrity(token: HANDLE) -> io::Result<()> {
  // Low integrity SID: S-1-16-4096.
  let sid_string = wide_from_str("S-1-16-4096");
  let mut sid: *mut std::ffi::c_void = std::ptr::null_mut();
  win32_bool(unsafe { ConvertStringSidToSidW(sid_string.as_ptr(), &mut sid) })?;
  if sid.is_null() {
    return Err(io::Error::new(
      io::ErrorKind::Other,
      "ConvertStringSidToSidW returned null SID",
    ));
  }
  let sid = LocalAllocSid { sid };

  let sid_len = unsafe { GetLengthSid(sid.sid) } as usize;
  let tml_len = std::mem::size_of::<TOKEN_MANDATORY_LABEL>() + sid_len;

  // `TOKEN_MANDATORY_LABEL` contains pointers, so ensure the backing buffer has at least pointer
  // alignment. `Vec<u8>` only guarantees 1-byte alignment.
  let word_count = (tml_len + std::mem::size_of::<usize>() - 1) / std::mem::size_of::<usize>();
  let mut buffer_words = vec![0usize; word_count];
  let buffer_ptr = buffer_words.as_mut_ptr().cast::<u8>();

  let tml_ptr = buffer_ptr.cast::<TOKEN_MANDATORY_LABEL>();
  let sid_ptr = unsafe { buffer_ptr.add(std::mem::size_of::<TOKEN_MANDATORY_LABEL>()) };
  unsafe {
    (*tml_ptr).Label.Attributes = SE_GROUP_INTEGRITY | SE_GROUP_INTEGRITY_ENABLED;
    (*tml_ptr).Label.Sid = sid_ptr.cast();
    std::ptr::copy_nonoverlapping(sid.sid.cast::<u8>(), sid_ptr, sid_len);
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
  win32_bool(ok)
}

fn win32_bool(value: BOOL) -> io::Result<()> {
  if value == 0 {
    Err(io::Error::last_os_error())
  } else {
    Ok(())
  }
}

fn wide_from_os(value: &OsStr) -> Vec<u16> {
  let mut wide: Vec<u16> = value.encode_wide().collect();
  wide.push(0);
  wide
}

fn wide_from_str(value: &str) -> Vec<u16> {
  let mut wide: Vec<u16> = value.encode_utf16().collect();
  wide.push(0);
  wide
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
  // Trailing backslashes before the closing quote need to be doubled.
  cmd.extend(std::iter::repeat('\\' as u16).take(backslashes * 2));
  cmd.push('"' as u16);
}

fn hresult_from_win32(err: u32) -> i32 {
  if err == 0 {
    return 0;
  }
  (0x8007_0000u32 | (err & 0xFFFF)) as i32
}

fn io_error_from_hresult(hr: i32) -> io::Error {
  let hr_u = hr as u32;
  if (hr_u & 0xFFFF_0000) == 0x8007_0000 {
    let code = (hr_u & 0xFFFF) as i32;
    return io::Error::from_raw_os_error(code);
  }
  io::Error::new(
    io::ErrorKind::Other,
    format!("windows HRESULT 0x{hr_u:08x}"),
  )
}

#[cfg(test)]
mod tests {
  use super::*;
  use std::sync::Mutex;

  static ENV_LOCK: Mutex<()> = Mutex::new(());

  #[test]
  fn sandbox_disabled_env_forces_none() {
    let _guard = ENV_LOCK.lock().unwrap();

    let prev_disable = std::env::var_os(ENV_DISABLE_RENDERER_SANDBOX);
    let prev_windows = std::env::var_os(ENV_WINDOWS_RENDERER_SANDBOX);

    std::env::set_var(ENV_DISABLE_RENDERER_SANDBOX, "1");
    std::env::remove_var(ENV_WINDOWS_RENDERER_SANDBOX);
    assert_eq!(requested_renderer_sandbox_level(), None);

    std::env::remove_var(ENV_DISABLE_RENDERER_SANDBOX);
    std::env::set_var(ENV_WINDOWS_RENDERER_SANDBOX, "off");
    assert_eq!(requested_renderer_sandbox_level(), None);

    match prev_disable {
      Some(value) => std::env::set_var(ENV_DISABLE_RENDERER_SANDBOX, value),
      None => std::env::remove_var(ENV_DISABLE_RENDERER_SANDBOX),
    }
    match prev_windows {
      Some(value) => std::env::set_var(ENV_WINDOWS_RENDERER_SANDBOX, value),
      None => std::env::remove_var(ENV_WINDOWS_RENDERER_SANDBOX),
    }
  }
}
