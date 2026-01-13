//! Windows renderer sandboxing primitives.
//!
//! This is best-effort and intended for a future multiprocess browser architecture where
//! renderer processes run with substantially reduced OS capabilities.
//!
//! The intended sandbox boundary is documented in `docs/windows_sandbox.md`. Keep that doc accurate
//! when changing any of:
//! - AppContainer / restricted-token spawning
//! - Job object limits
//! - handle inheritance allowlisting
//! - environment-variable escape hatches
//!
//! ## Environment sanitization
//!
//! `CreateProcessW` and `CreateProcessAsUserW` inherit the parent's environment when
//! `lpEnvironment` is null. Browser processes often contain secrets in environment variables, so
//! sandboxed renderer children must not inherit the full environment by default.
//!
//! This module therefore builds an explicit UTF-16 environment block containing only a small
//! allowlist of variables required for basic runtime correctness, and passes it to the Windows
//! process creation APIs. In particular, it overrides `TEMP`/`TMP` to a sandbox-accessible temp
//! directory (in AppContainer mode this is typically `GetAppContainerFolderPath(AppContainerSid)\Temp`,
//! falling back to `C:\Windows\Temp` when the AppContainer storage folder cannot be queried).
//!
//! Set `FASTR_WINDOWS_SANDBOX_INHERIT_ENV=1` to opt back into inheriting the full environment for
//! debugging.

use std::ffi::{c_void, OsStr, OsString};
use std::io;
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle, RawHandle};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use tempfile::TempDir;
use win_sandbox::mitigations;

pub mod appcontainer;

/// Debug escape hatch: disable the renderer sandbox (INSECURE).
///
/// On Windows this disables AppContainer/restricted-token sandboxing. Other platforms may interpret
/// the same env var for their own renderer sandboxes.
const ENV_DISABLE_RENDERER_SANDBOX: &str = "FASTR_DISABLE_RENDERER_SANDBOX";

/// Legacy/alternative spelling for disabling the Windows renderer sandbox.
///
/// Accepted values:
/// - `off`, `0`, `false`, `no` (case-insensitive) => disable sandboxing
/// - any other non-empty value => leave sandboxing enabled (default)
const ENV_WINDOWS_RENDERER_SANDBOX: &str = "FASTR_WINDOWS_RENDERER_SANDBOX";

/// Enable verbose sandbox logging (primarily for Windows AppContainer spawn debugging).
const ENV_LOG_SANDBOX: &str = "FASTR_LOG_SANDBOX";

/// Explicit opt-in: allow the renderer to run without the full Windows sandbox when required
/// primitives are unavailable or sandbox setup fails.
///
/// This is intended for developer convenience on unsupported Windows versions / CI environments.
/// It is **never** enabled by default.
const ENV_ALLOW_UNSANDBOXED_RENDERER: &str = "FASTR_ALLOW_UNSANDBOXED_RENDERER";

/// Debug escape hatch: allow sandboxed renderer children to inherit the full parent environment.
///
/// This is intentionally Windows-only (the variable is ignored on other platforms).
const ENV_INHERIT_RENDERER_ENV: &str = "FASTR_WINDOWS_SANDBOX_INHERIT_ENV";

/// Conservative fallback CWD when the AppContainer profile storage folder cannot be queried.
const FALLBACK_APPCONTAINER_CWD: &str = r"C:\Windows\System32";

/// Conservative fallback temp directory when the AppContainer profile storage folder cannot be queried.
const FALLBACK_APPCONTAINER_TEMP: &str = r"C:\Windows\Temp";

// STARTUPINFOEX attribute value:
// ProcThreadAttributeValue(7, FALSE, TRUE, FALSE) → 0x0002_0007.
const PROC_THREAD_ATTRIBUTE_MITIGATION_POLICY: usize = 0x0002_0007;

// STARTUPINFOEX attribute value:
// ProcThreadAttributeValue(15, FALSE, TRUE, FALSE) → 0x0002_000F.
//
// When set to `PROCESS_CREATION_ALL_APPLICATION_PACKAGES_POLICY_BLOCK`, the created AppContainer
// token does **not** include the broad `ALL APPLICATION PACKAGES` group (S-1-15-2-1). This reduces
// default access to resources that are ACL'd to that group.
const PROC_THREAD_ATTRIBUTE_ALL_APPLICATION_PACKAGES_POLICY: usize = 0x0002_000F;

// Value for `PROC_THREAD_ATTRIBUTE_ALL_APPLICATION_PACKAGES_POLICY` (winbase.h).
//
// `PROCESS_CREATION_ALL_APPLICATION_PACKAGES_POLICY_BLOCK` removes the `ALL APPLICATION PACKAGES`
// group from the created token.
const PROCESS_CREATION_ALL_APPLICATION_PACKAGES_POLICY_BLOCK: u32 = 1;

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
/// - `Some(..)` indicates the preferred sandbox mode; callers may only apply downgrade paths when
///   explicitly opted in (see [`ENV_ALLOW_UNSANDBOXED_RENDERER`]).
pub(crate) fn requested_renderer_sandbox_level() -> Option<WindowsRendererSandboxLevel> {
  if renderer_sandbox_disabled_via_env() {
    log_sandbox_disabled_once();
    return None;
  }

  Some(WindowsRendererSandboxLevel::AppContainer)
}

fn allow_unsandboxed_renderer_via_env() -> bool {
  matches!(
    std::env::var_os(ENV_ALLOW_UNSANDBOXED_RENDERER).as_deref(),
    Some(v) if v == OsStr::new("1")
  )
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

fn should_inherit_renderer_environment() -> bool {
  env_var_truthy(std::env::var_os(ENV_INHERIT_RENDERER_ENV).as_deref())
}

fn log_sandbox_disabled_once() {
  static LOGGED: OnceLock<()> = OnceLock::new();
  LOGGED.get_or_init(|| {
    eprintln!(
      "warning: Windows renderer sandbox is DISABLED (debug escape hatch; INSECURE). \
Set {ENV_DISABLE_RENDERER_SANDBOX}=0/1 or {ENV_WINDOWS_RENDERER_SANDBOX}=off to control this."
    );
  });
}

fn log_sandbox_debug(msg: &str) {
  if cfg!(debug_assertions) || env_var_truthy(std::env::var_os(ENV_LOG_SANDBOX).as_deref()) {
    eprintln!("{msg}");
  }
}

use windows_sys::Win32::Foundation::{
  BOOL, ERROR_ACCESS_DENIED, ERROR_ALREADY_EXISTS, ERROR_SUCCESS, FALSE, HANDLE, TRUE,
};
use windows_sys::Win32::Security::Authorization::{
  GetNamedSecurityInfoW, SetEntriesInAclW, SetNamedSecurityInfoW, EXPLICIT_ACCESS_W, GRANT_ACCESS,
  NO_MULTIPLE_TRUSTEE, SE_FILE_OBJECT, TRUSTEE_IS_SID, TRUSTEE_IS_UNKNOWN, TRUSTEE_W,
};
use windows_sys::Win32::Security::{
  ConvertStringSidToSidW, CreateRestrictedToken, FreeSid, GetLengthSid, OpenProcessToken,
  SetTokenInformation, TokenIntegrityLevel, DISABLE_MAX_PRIVILEGE, SECURITY_CAPABILITIES,
  SE_GROUP_INTEGRITY, SE_GROUP_INTEGRITY_ENABLED, TOKEN_ADJUST_DEFAULT, TOKEN_ASSIGN_PRIMARY,
  TOKEN_DUPLICATE, TOKEN_INFORMATION_CLASS, TOKEN_MANDATORY_LABEL, TOKEN_QUERY,
};
use windows_sys::Win32::Storage::FileSystem::{FILE_GENERIC_EXECUTE, FILE_GENERIC_READ};
use windows_sys::Win32::System::JobObjects::{
  AssignProcessToJobObject, CreateJobObjectW, IsProcessInJob, JobObjectBasicUIRestrictions,
  JobObjectExtendedLimitInformation, SetInformationJobObject, JOBOBJECT_BASIC_UI_RESTRICTIONS,
  JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOB_OBJECT_LIMIT_ACTIVE_PROCESS,
  JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE, JOB_OBJECT_UILIMIT_DESKTOP, JOB_OBJECT_UILIMIT_DISPLAYSETTINGS,
  JOB_OBJECT_UILIMIT_EXITWINDOWS, JOB_OBJECT_UILIMIT_GLOBALATOMS, JOB_OBJECT_UILIMIT_HANDLES,
  JOB_OBJECT_UILIMIT_READCLIPBOARD, JOB_OBJECT_UILIMIT_SYSTEMPARAMETERS,
  JOB_OBJECT_UILIMIT_WRITECLIPBOARD,
};
use windows_sys::Win32::System::Memory::{GetProcessHeap, HeapAlloc, HeapFree, LocalFree};
use windows_sys::Win32::System::Threading::{
  CreateProcessAsUserW, CreateProcessW, DeleteProcThreadAttributeList, GetCurrentProcess,
  GetExitCodeProcess, InitializeProcThreadAttributeList, ResumeThread, TerminateProcess,
  UpdateProcThreadAttribute, WaitForSingleObject, CREATE_BREAKAWAY_FROM_JOB, CREATE_SUSPENDED,
  CREATE_UNICODE_ENVIRONMENT, EXTENDED_STARTUPINFO_PRESENT, PROCESS_INFORMATION,
  LPPROC_THREAD_ATTRIBUTE_LIST, PROC_THREAD_ATTRIBUTE_HANDLE_LIST,
  PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES, STARTUPINFOEXW, STARTUPINFOW,
};

// `accctrl.h` defines `NO_INHERITANCE` as 0, but `windows-sys` does not currently export it.
const NO_INHERITANCE: u32 = 0;

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
  /// Job object used to enforce kill-on-close + process-count limits.
  ///
  /// When the parent process is already inside a Windows Job that disallows breakaway/nested jobs
  /// (common in CI/supervisors), `AssignProcessToJobObject` can fail. In that case we still spawn
  /// the process in an AppContainer/restricted token, but `job` is `None` to indicate the job
  /// containment guarantees are not enforced.
  pub job: Option<JobObject>,
  pub level: WindowsSandboxLevel,
  // Keep any relocated AppContainer executable alive for the lifetime of the child handle.
  _temp_dir: Option<TempDir>,
}

impl SandboxedChild {
  /// Wait for the child process to exit, returning its raw exit code.
  pub fn wait(self) -> io::Result<u32> {
    let process = self.process.as_raw_handle() as HANDLE;
    // `INFINITE` and `WAIT_FAILED` are both `u32::MAX` in the Win32 headers.
    let rc = unsafe { WaitForSingleObject(process, u32::MAX) };
    if rc == u32::MAX {
      return Err(io::Error::last_os_error());
    }
    if rc != 0 {
      return Err(io::Error::new(
        io::ErrorKind::Other,
        format!("WaitForSingleObject returned unexpected status {rc}"),
      ));
    }

    let mut code: u32 = 0;
    win32_bool(unsafe { GetExitCodeProcess(process, &mut code as *mut u32) })?;
    Ok(code)
  }
}

/// Additional configuration for spawning Windows sandboxed child processes.
#[derive(Debug, Clone, Copy)]
pub struct SpawnConfig {
  /// When `true`, the spawned AppContainer token has the broad
  /// `ALL APPLICATION PACKAGES` group (SID `S-1-15-2-1`) removed via
  /// `PROC_THREAD_ATTRIBUTE_ALL_APPLICATION_PACKAGES_POLICY`.
  ///
  /// This is a defense-in-depth hardening measure: some system objects are ACL'd to
  /// `ALL APPLICATION PACKAGES`, so removing the group reduces ambient access for the renderer.
  ///
  /// Compatibility note: on Windows builds that do not support this attribute, the spawner
  /// automatically retries without it.
  pub all_application_packages_hardened: bool,
}

impl Default for SpawnConfig {
  fn default() -> Self {
    Self {
      // Enable by default for the renderer sandbox preset. If this turns out to be incompatible
      // with some Windows builds (e.g. font loading), callers can disable it explicitly.
      all_application_packages_hardened: true,
    }
  }
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
  list: LPPROC_THREAD_ATTRIBUTE_LIST,
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
    let list = alloc.cast::<std::ffi::c_void>();
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
        LocalFree(self.sid as _);
      }
    }
  }
}

fn current_process_in_job() -> io::Result<bool> {
  let mut in_job: BOOL = FALSE;
  win32_bool(unsafe { IsProcessInJob(GetCurrentProcess(), 0, &mut in_job) })?;
  Ok(in_job != FALSE)
}

fn spawn_with_optional_breakaway<F>(
  parent_in_job: bool,
  create_process: &mut F,
  base_flags: u32,
) -> io::Result<(PROCESS_INFORMATION, bool)>
where
  F: FnMut(u32) -> io::Result<PROCESS_INFORMATION>,
{
  if !parent_in_job {
    return create_process(base_flags).map(|pi| (pi, false));
  }

  match create_process(base_flags | CREATE_BREAKAWAY_FROM_JOB) {
    Ok(pi) => Ok((pi, true)),
    Err(err) => {
      if err.raw_os_error() != Some(ERROR_ACCESS_DENIED as i32) {
        return Err(err);
      }
      log_sandbox_debug(
        "windows sandbox: CreateProcess* with CREATE_BREAKAWAY_FROM_JOB returned ERROR_ACCESS_DENIED; retrying without breakaway",
      );
      create_process(base_flags).map(|pi| (pi, false))
    }
  }
}

fn mitigation_policy_attribute_unsupported(err: &io::Error) -> bool {
  matches!(err.raw_os_error(), Some(code) if code == 50 || code == 87)
}

pub fn spawn_sandboxed(
  exe: &Path,
  args: &[OsString],
  inherit_handles: &[RawHandle],
) -> io::Result<SandboxedChild> {
  spawn_sandboxed_with_config(exe, args, inherit_handles, SpawnConfig::default())
}

pub fn spawn_sandboxed_with_config(
  exe: &Path,
  args: &[OsString],
  inherit_handles: &[RawHandle],
  config: SpawnConfig,
) -> io::Result<SandboxedChild> {
  let mitigation_policy = mitigations::renderer_mitigation_policy();
  let allow_fallback = allow_unsandboxed_renderer_via_env();
  let parent_in_job = match current_process_in_job() {
    Ok(value) => value,
    Err(err) => {
      log_sandbox_debug(&format!(
        "windows sandbox: IsProcessInJob(GetCurrentProcess) failed ({err}); assuming parent is not in a job"
      ));
      false
    }
  };

  let Some(requested) = requested_renderer_sandbox_level() else {
    // Debug escape hatch: sandbox explicitly disabled. This is always allowed.
    return spawn_unsandboxed(
      exe,
      args,
      inherit_handles,
      parent_in_job,
      mitigation_policy,
      true,
    );
  };

  match requested {
    WindowsRendererSandboxLevel::AppContainer => match spawn_appcontainer(
      exe,
      args,
      inherit_handles,
      parent_in_job,
      mitigation_policy,
      config.all_application_packages_hardened,
      allow_fallback,
    ) {
      Ok(child) => Ok(child),
      Err(appcontainer_err) => {
        if !allow_fallback {
          return Err(io::Error::new(
            appcontainer_err.kind(),
            format!(
              "windows renderer sandbox unavailable ({appcontainer_err}); \
set {ENV_ALLOW_UNSANDBOXED_RENDERER}=1 to allow running without the full Windows sandbox"
            ),
          ));
        }

        eprintln!(
          "warning: Windows AppContainer sandbox failed ({appcontainer_err}); \
falling back to weaker sandboxing because {ENV_ALLOW_UNSANDBOXED_RENDERER}=1 is set"
        );
        match spawn_restricted_token(
          exe,
          args,
          inherit_handles,
          parent_in_job,
          mitigation_policy,
          true,
        ) {
          Ok(child) => Ok(child),
          Err(restricted_err) => match spawn_unsandboxed(
            exe,
            args,
            inherit_handles,
            parent_in_job,
            mitigation_policy,
            true,
          ) {
            Ok(child) => {
              eprintln!(
                "warning: Windows renderer sandbox failed (appcontainer={appcontainer_err}, restricted_token={restricted_err}); \
spawning UNSANDBOXED process (job limits may still apply)"
              );
              Ok(child)
            }
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
    WindowsRendererSandboxLevel::RestrictedToken => match spawn_restricted_token(
      exe,
      args,
      inherit_handles,
      parent_in_job,
      mitigation_policy,
      allow_fallback,
    ) {
      Ok(child) => Ok(child),
      Err(restricted_err) => {
        if !allow_fallback {
          return Err(io::Error::new(
            restricted_err.kind(),
            format!(
              "windows renderer sandbox unavailable (restricted-token spawn failed: {restricted_err}); \
 set {ENV_ALLOW_UNSANDBOXED_RENDERER}=1 to allow running without the full Windows sandbox"
            ),
          ));
        }

        match spawn_unsandboxed(
          exe,
          args,
          inherit_handles,
          parent_in_job,
          mitigation_policy,
          true,
        ) {
          Ok(child) => {
            eprintln!(
              "warning: Windows restricted-token sandbox failed ({restricted_err}); \
 spawning UNSANDBOXED process because {ENV_ALLOW_UNSANDBOXED_RENDERER}=1 is set"
            );
            Ok(child)
          }
          Err(unsandboxed_err) => Err(io::Error::new(
            unsandboxed_err.kind(),
            format!(
              "failed to spawn child process (restricted_token={restricted_err}, unsandboxed={unsandboxed_err})"
            ),
          )),
        }
      }
    },
  }
}

fn spawn_appcontainer(
  exe: &Path,
  args: &[OsString],
  inherit_handles: &[RawHandle],
  parent_in_job: bool,
  mitigation_policy: u64,
  all_application_packages_hardened: bool,
  allow_jobless: bool,
) -> io::Result<SandboxedChild> {
  let job = JobObject::new()?;
  let name = wide_from_str("FastRender.Renderer");
  let sid = AppContainerSid::new(&name)?;

  // Ensure the sandboxed child starts in a directory it can access and has a writable temp dir.
  //
  // By default, the child would inherit the parent's CWD and TMP/TEMP, which frequently point at
  // user profile paths that an AppContainer cannot access (causing surprising failures in
  // dependencies that write temp files, scan fonts, etc).
  let (current_dir, sandbox_temp_dir) = resolve_appcontainer_working_dirs(sid.sid);
  let current_dir_wide = wide_from_os(current_dir.as_os_str());
  let env_block = build_renderer_environment_block_for_temp_dir(&sandbox_temp_dir);
  let env_ptr = env_block
    .as_ref()
    .map_or(std::ptr::null(), |block| block.as_ptr() as *const c_void);

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

  let mut all_packages_policy_value = PROCESS_CREATION_ALL_APPLICATION_PACKAGES_POLICY_BLOCK;
  let mut mitigation_policy_value = mitigation_policy;

  let base_attribute_count = 1 + u32::from(!handles.is_empty());
  let attribute_count_with_aap = base_attribute_count + u32::from(all_application_packages_hardened);
  let attribute_count_with_aap_and_mitigations =
    attribute_count_with_aap + u32::from(mitigation_policy_value != 0);

  // Build attribute list variants up-front so CreateProcess retries can switch between them without
  // reallocating the whole list under error paths.
  let mut attrs_without_mitigations = AttributeList::new(attribute_count_with_aap)?;
  let mut attrs_without_aap: Option<AttributeList> = None;
  let mut attrs_with_mitigations: Option<AttributeList> = None;
  let mut attrs_with_mitigations_no_aap: Option<AttributeList> = None;

  {
    // Helper closure for populating a STARTUPINFOEX attribute list.
    //
    // This closure borrows `handles` mutably (for `as_mut_ptr()`), so keep it scoped to this block
    // to avoid borrow-checker conflicts with later uses of `handles`.
    let mut init_attrs_base = |attrs: &mut AttributeList, include_aap: bool| -> io::Result<()> {
      attrs.update_raw(
        PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES,
        std::ptr::addr_of_mut!(capabilities).cast(),
        std::mem::size_of::<SECURITY_CAPABILITIES>(),
      )?;

      if include_aap {
        // Best-effort: older Windows builds may not support this attribute. If the OS rejects it as
        // invalid/unsupported, continue without the hardening policy.
        if let Err(err) = attrs.update_raw(
          PROC_THREAD_ATTRIBUTE_ALL_APPLICATION_PACKAGES_POLICY,
          std::ptr::addr_of_mut!(all_packages_policy_value).cast(),
          std::mem::size_of::<u32>(),
        ) {
          if mitigation_policy_attribute_unsupported(&err) {
            log_sandbox_debug(&format!(
              "windows sandbox: UpdateProcThreadAttribute(PROC_THREAD_ATTRIBUTE_ALL_APPLICATION_PACKAGES_POLICY) rejected by OS ({err}); continuing without ALL APPLICATION PACKAGES hardening"
            ));
          } else {
            return Err(err);
          }
        }
      }

      if !handles.is_empty() {
        attrs.update_raw(
          PROC_THREAD_ATTRIBUTE_HANDLE_LIST,
          handles.as_mut_ptr().cast(),
          handles.len() * std::mem::size_of::<HANDLE>(),
        )?;
      }
      Ok(())
    };

    init_attrs_base(&mut attrs_without_mitigations, all_application_packages_hardened)?;

    if all_application_packages_hardened {
      let mut attrs = AttributeList::new(base_attribute_count)?;
      init_attrs_base(&mut attrs, false)?;
      attrs_without_aap = Some(attrs);
    }

    if mitigation_policy_value != 0 {
      let mut attrs = AttributeList::new(attribute_count_with_aap_and_mitigations)?;
      init_attrs_base(&mut attrs, all_application_packages_hardened)?;
      match attrs.update_raw(
        PROC_THREAD_ATTRIBUTE_MITIGATION_POLICY,
        std::ptr::addr_of_mut!(mitigation_policy_value).cast(),
        std::mem::size_of::<u64>(),
      ) {
        Ok(()) => attrs_with_mitigations = Some(attrs),
        Err(err) if mitigation_policy_attribute_unsupported(&err) => {
          log_sandbox_debug(&format!(
            "windows sandbox: UpdateProcThreadAttribute rejected mitigation policy attribute ({err}); continuing without mitigations"
          ));
        }
        Err(err) => return Err(err),
      }

      if all_application_packages_hardened {
        let mut attrs = AttributeList::new(base_attribute_count + 1)?;
        init_attrs_base(&mut attrs, false)?;
        match attrs.update_raw(
          PROC_THREAD_ATTRIBUTE_MITIGATION_POLICY,
          std::ptr::addr_of_mut!(mitigation_policy_value).cast(),
          std::mem::size_of::<u64>(),
        ) {
          Ok(()) => attrs_with_mitigations_no_aap = Some(attrs),
          Err(err) if mitigation_policy_attribute_unsupported(&err) => {}
          Err(err) => return Err(err),
        }
      }
    }
  }

  let inherit = if handles.is_empty() { FALSE } else { TRUE };
  let env_flags = if env_block.is_some() {
    CREATE_UNICODE_ENVIRONMENT
  } else {
    0
  };
  let base_flags = CREATE_SUSPENDED | EXTENDED_STARTUPINFO_PRESENT | env_flags;

  let mut create_process = |image: &Path,
                            flags: u32,
                            current_dir: Option<&Path>|
   -> io::Result<PROCESS_INFORMATION> {
    let application_name = wide_from_os(image.as_os_str());

    // Always set a known-good current directory:
    // - AppContainer tokens often cannot access the parent's CWD (e.g. a repo/build directory).
    // - Many dependencies probe or use relative paths (including `.`) during startup.
    //
    // When the executable relocation/ACL remediation path is taken, we allow overriding the CWD to
    // the temp directory we control.
    let current_dir_w = current_dir.map(|dir| wide_from_os(dir.as_os_str()));
    let current_dir_ptr = match current_dir_w.as_ref() {
      Some(wide) => wide.as_ptr(),
      None => current_dir_wide.as_ptr(),
    };

    let mut create_process_with_attrs =
      |attr_list: *mut PROC_THREAD_ATTRIBUTE_LIST| -> io::Result<PROCESS_INFORMATION> {
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
            env_ptr,
            current_dir_ptr,
            std::ptr::addr_of_mut!(startup).cast::<STARTUPINFOW>(),
            &mut pi,
          )
        };
        if ok == 0 {
          return Err(io::Error::last_os_error());
        }
        Ok(pi)
      };

    // Try CreateProcess with the strongest available configuration first (mitigations + AAP
    // hardening), then fall back on older/unsupported Windows builds that reject particular
    // `STARTUPINFOEX` attributes.
    //
    // `ERROR_NOT_SUPPORTED (50)` and `ERROR_INVALID_PARAMETER (87)` are the most common errors when
    // an attribute is not supported by the host OS.
    let is_optional_attr_error = |err: &io::Error| {
      matches!(err.raw_os_error(), Some(code) if code == 50 || code == 87)
    };

    let mut attempts: Vec<(*mut PROC_THREAD_ATTRIBUTE_LIST, &'static str)> = Vec::new();
    if let Some(attrs) = attrs_with_mitigations.as_ref() {
      attempts.push((attrs.list, "mitigations + AAP hardening"));
    }
    attempts.push((attrs_without_mitigations.list, "AAP hardening"));
    if let Some(attrs) = attrs_with_mitigations_no_aap.as_ref() {
      attempts.push((attrs.list, "mitigations (no AAP hardening)"));
    }
    if let Some(attrs) = attrs_without_aap.as_ref() {
      attempts.push((attrs.list, "no mitigations, no AAP hardening"));
    }

    let mut last_optional_err: Option<io::Error> = None;
    for (list, label) in attempts {
      match create_process_with_attrs(list) {
        Ok(pi) => return Ok(pi),
        Err(err) => {
          if is_optional_attr_error(&err) {
            log_sandbox_debug(&format!(
              "windows sandbox: CreateProcessW rejected startup attributes ({label}): {err}; retrying with weaker attribute set"
            ));
            last_optional_err = Some(err);
            continue;
          }
          return Err(err);
        }
      }
    }
    Err(last_optional_err.unwrap_or_else(|| {
      io::Error::new(
        io::ErrorKind::Other,
        "CreateProcessW failed with unsupported startup attributes",
      )
    }))
  };

  let mut create_process_with_job_strategy =
    |image: &Path, current_dir: Option<&Path>| -> io::Result<(PROCESS_INFORMATION, bool)> {
      if !parent_in_job {
        return create_process(image, base_flags, current_dir).map(|pi| (pi, false));
      }

      match create_process(
        image,
        base_flags | CREATE_BREAKAWAY_FROM_JOB,
        current_dir,
      ) {
        Ok(pi) => Ok((pi, true)),
        Err(err) => {
          if err.raw_os_error() != Some(ERROR_ACCESS_DENIED as i32) {
            return Err(err);
          }
          log_sandbox_debug(&format!(
            "windows sandbox: CreateProcessW with CREATE_BREAKAWAY_FROM_JOB returned ERROR_ACCESS_DENIED for {}; retrying without breakaway",
            image.display()
          ));
          create_process(image, base_flags, current_dir).map(|pi| (pi, false))
        }
      }
    };

  match create_process_with_job_strategy(exe, None) {
    Ok((pi, used_breakaway)) => {
      return finish_spawn(
        job,
        pi,
        WindowsSandboxLevel::AppContainer,
        None,
        parent_in_job,
        used_breakaway,
        allow_jobless,
      );
    }
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

  // Keep the sandboxed process's working directory inside the AppContainer profile folder, even if
  // we had to relocate the executable to a host temp dir to grant read/execute ACLs.
  match create_process_with_job_strategy(&relocated, None) {
    Ok((pi, used_breakaway)) => finish_spawn(
      job,
      pi,
      WindowsSandboxLevel::AppContainer,
      Some(temp_dir),
      parent_in_job,
      used_breakaway,
      allow_jobless,
    ),
    Err(err) => {
      log_sandbox_debug(&format!(
        "windows sandbox: AppContainer retry after relocation failed ({err})"
      ));
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
    unsafe { LocalFree(sd as _) };
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
    LocalFree(sd as _);
    LocalFree(new_dacl as _);
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
  parent_in_job: bool,
  mitigation_policy: u64,
  allow_jobless: bool,
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
  let env_block = build_renderer_environment_block();
  let env_ptr = env_block
    .as_ref()
    .map_or(std::ptr::null(), |block| block.as_ptr() as *const c_void);
  let env_flags = if env_block.is_some() {
    CREATE_UNICODE_ENVIRONMENT
  } else {
    0
  };

  // If `lpCurrentDirectory` is NULL, Windows inherits the parent's current directory. For a
  // low-integrity restricted token that directory may be inaccessible, causing `CreateProcessAsUserW`
  // to fail with `ERROR_ACCESS_DENIED`.
  //
  // Prefer the executable's parent directory (if the image is loadable, the directory is generally
  // traversable too). Fall back to a conservative system directory.
  let current_dir = exe
    .parent()
    .unwrap_or_else(|| Path::new(FALLBACK_APPCONTAINER_CWD));
  let current_dir_wide = wide_from_os(current_dir.as_os_str());

  let handles: Vec<HANDLE> = inherit_handles
    .iter()
    .copied()
    .map(|h| h as HANDLE)
    .collect();
  let mut handles = handles;

  let mut mitigation_policy_value = mitigation_policy;

  let (pi, used_breakaway) = if handles.is_empty() {
    let mut attrs_with_mitigations: Option<AttributeList> = None;
    if mitigation_policy_value != 0 {
      let mut attrs = AttributeList::new(1)?;
      if let Err(err) = attrs.update_raw(
        PROC_THREAD_ATTRIBUTE_MITIGATION_POLICY,
        std::ptr::addr_of_mut!(mitigation_policy_value).cast(),
        std::mem::size_of::<u64>(),
      ) {
        if mitigation_policy_attribute_unsupported(&err) {
          log_sandbox_debug(&format!(
            "windows sandbox: UpdateProcThreadAttribute rejected mitigation policy attribute ({err}); continuing without mitigations"
          ));
          mitigation_policy_value = 0;
        } else {
          return Err(err);
        }
      } else {
        attrs_with_mitigations = Some(attrs);
      }
    }

    if let Some(attrs) = attrs_with_mitigations {
      let base_flags = CREATE_SUSPENDED | EXTENDED_STARTUPINFO_PRESENT | env_flags;

      let mut create_process = |flags: u32| -> io::Result<PROCESS_INFORMATION> {
        let mut cmdline = build_command_line(exe, args);

        let mut startup_ex: STARTUPINFOEXW = unsafe { std::mem::zeroed() };
        startup_ex.StartupInfo.cb = std::mem::size_of::<STARTUPINFOEXW>() as u32;
        startup_ex.lpAttributeList = attrs.list;

        let mut pi: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };
        let ok = unsafe {
          CreateProcessAsUserW(
            restricted.as_raw_handle() as HANDLE,
            application_name.as_ptr(),
            cmdline.as_mut_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            FALSE,
            flags,
            env_ptr,
            current_dir_wide.as_ptr(),
            std::ptr::addr_of_mut!(startup_ex).cast::<STARTUPINFOW>(),
            &mut pi,
          )
        };
        if ok == 0 {
          let err = io::Error::last_os_error();
          if mitigation_policy_attribute_unsupported(&err) {
            log_sandbox_debug(&format!(
              "windows sandbox: CreateProcessAsUserW rejected mitigation policy attribute ({err}); retrying without mitigations"
            ));
            let mut startup: STARTUPINFOW = unsafe { std::mem::zeroed() };
            startup.cb = std::mem::size_of::<STARTUPINFOW>() as u32;
            let mut pi: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };
            let mut cmdline_fallback = build_command_line(exe, args);
            let ok = unsafe {
              CreateProcessAsUserW(
                restricted.as_raw_handle() as HANDLE,
                application_name.as_ptr(),
                cmdline_fallback.as_mut_ptr(),
                std::ptr::null(),
                std::ptr::null(),
                FALSE,
                flags & !EXTENDED_STARTUPINFO_PRESENT,
                env_ptr,
                current_dir_wide.as_ptr(),
                &mut startup,
                &mut pi,
              )
            };
            if ok == 0 {
              return Err(io::Error::last_os_error());
            }
            return Ok(pi);
          }
          return Err(err);
        }
        Ok(pi)
      };

      spawn_with_optional_breakaway(parent_in_job, &mut create_process, base_flags)?
    } else {
      let mut startup: STARTUPINFOW = unsafe { std::mem::zeroed() };
      startup.cb = std::mem::size_of::<STARTUPINFOW>() as u32;

      let mut create_process = |flags: u32| -> io::Result<PROCESS_INFORMATION> {
        let mut cmdline = build_command_line(exe, args);
        let mut pi: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };
        let ok = unsafe {
          CreateProcessAsUserW(
            restricted.as_raw_handle() as HANDLE,
            application_name.as_ptr(),
            cmdline.as_mut_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            FALSE,
            flags,
            env_ptr,
            current_dir_wide.as_ptr(),
            &mut startup,
            &mut pi,
          )
        };
        if ok == 0 {
          return Err(io::Error::last_os_error());
        }
        Ok(pi)
      };

      spawn_with_optional_breakaway(
        parent_in_job,
        &mut create_process,
        CREATE_SUSPENDED | env_flags,
      )?
    }
  } else {
    let mut attrs_without_mitigations = AttributeList::new(1)?;
    attrs_without_mitigations.update_raw(
      PROC_THREAD_ATTRIBUTE_HANDLE_LIST,
      handles.as_mut_ptr().cast(),
      handles.len() * std::mem::size_of::<HANDLE>(),
    )?;

    let attrs_with_mitigations = if mitigation_policy_value != 0 {
      let mut attrs = AttributeList::new(2)?;
      attrs.update_raw(
        PROC_THREAD_ATTRIBUTE_HANDLE_LIST,
        handles.as_mut_ptr().cast(),
        handles.len() * std::mem::size_of::<HANDLE>(),
      )?;
      if let Err(err) = attrs.update_raw(
        PROC_THREAD_ATTRIBUTE_MITIGATION_POLICY,
        std::ptr::addr_of_mut!(mitigation_policy_value).cast(),
        std::mem::size_of::<u64>(),
      ) {
        if mitigation_policy_attribute_unsupported(&err) {
          log_sandbox_debug(&format!(
            "windows sandbox: UpdateProcThreadAttribute rejected mitigation policy attribute ({err}); continuing without mitigations"
          ));
          None
        } else {
          return Err(err);
        }
      } else {
        Some(attrs)
      }
    } else {
      None
    };

    let base_flags = CREATE_SUSPENDED | EXTENDED_STARTUPINFO_PRESENT | env_flags;

    let mut create_process = |flags: u32| -> io::Result<PROCESS_INFORMATION> {
      let mut create_process_with_attrs =
        |attr_list: *mut PROC_THREAD_ATTRIBUTE_LIST| -> io::Result<PROCESS_INFORMATION> {
          let mut cmdline = build_command_line(exe, args);
          let mut startup_ex: STARTUPINFOEXW = unsafe { std::mem::zeroed() };
          startup_ex.StartupInfo.cb = std::mem::size_of::<STARTUPINFOEXW>() as u32;
          startup_ex.lpAttributeList = attr_list;

          let mut pi: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };
          let ok = unsafe {
            CreateProcessAsUserW(
              restricted.as_raw_handle() as HANDLE,
              application_name.as_ptr(),
              cmdline.as_mut_ptr(),
              std::ptr::null(),
              std::ptr::null(),
              TRUE,
              flags,
              env_ptr,
              current_dir_wide.as_ptr(),
              std::ptr::addr_of_mut!(startup_ex).cast::<STARTUPINFOW>(),
              &mut pi,
            )
          };
          if ok == 0 {
            return Err(io::Error::last_os_error());
          }
          Ok(pi)
        };

      if let Some(attrs) = attrs_with_mitigations.as_ref() {
        match create_process_with_attrs(attrs.list) {
          Ok(pi) => Ok(pi),
          Err(err) => {
            if mitigation_policy_attribute_unsupported(&err) {
              log_sandbox_debug(&format!(
                "windows sandbox: CreateProcessAsUserW rejected mitigation policy attribute ({err}); retrying without mitigations"
              ));
              create_process_with_attrs(attrs_without_mitigations.list)
            } else {
              Err(err)
            }
          }
        }
      } else {
        create_process_with_attrs(attrs_without_mitigations.list)
      }
    };

    spawn_with_optional_breakaway(parent_in_job, &mut create_process, base_flags)?
  };

  finish_spawn(
    job,
    pi,
    WindowsSandboxLevel::RestrictedToken,
    None,
    parent_in_job,
    used_breakaway,
    allow_jobless,
  )
}

fn spawn_unsandboxed(
  exe: &Path,
  args: &[OsString],
  inherit_handles: &[RawHandle],
  parent_in_job: bool,
  mitigation_policy: u64,
  allow_jobless: bool,
) -> io::Result<SandboxedChild> {
  let job = JobObject::new()?;
  let application_name = wide_from_os(exe.as_os_str());
  let env_block = build_renderer_environment_block();
  let env_ptr = env_block
    .as_ref()
    .map_or(std::ptr::null(), |block| block.as_ptr() as *const c_void);
  let env_flags = if env_block.is_some() {
    CREATE_UNICODE_ENVIRONMENT
  } else {
    0
  };

  let handles: Vec<HANDLE> = inherit_handles
    .iter()
    .copied()
    .map(|h| h as HANDLE)
    .collect();
  let mut handles = handles;

  let mut mitigation_policy_value = mitigation_policy;

  let (pi, used_breakaway) = if handles.is_empty() {
    let mut attrs_with_mitigations: Option<AttributeList> = None;
    if mitigation_policy_value != 0 {
      let mut attrs = AttributeList::new(1)?;
      if let Err(err) = attrs.update_raw(
        PROC_THREAD_ATTRIBUTE_MITIGATION_POLICY,
        std::ptr::addr_of_mut!(mitigation_policy_value).cast(),
        std::mem::size_of::<u64>(),
      ) {
        if mitigation_policy_attribute_unsupported(&err) {
          log_sandbox_debug(&format!(
            "windows sandbox: UpdateProcThreadAttribute rejected mitigation policy attribute ({err}); continuing without mitigations"
          ));
          mitigation_policy_value = 0;
        } else {
          return Err(err);
        }
      } else {
        attrs_with_mitigations = Some(attrs);
      }
    }

    if let Some(attrs) = attrs_with_mitigations {
      let base_flags = CREATE_SUSPENDED | EXTENDED_STARTUPINFO_PRESENT | env_flags;

      let mut create_process = |flags: u32| -> io::Result<PROCESS_INFORMATION> {
        let mut cmdline = build_command_line(exe, args);

        let mut startup_ex: STARTUPINFOEXW = unsafe { std::mem::zeroed() };
        startup_ex.StartupInfo.cb = std::mem::size_of::<STARTUPINFOEXW>() as u32;
        startup_ex.lpAttributeList = attrs.list;

        let mut pi: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };
        let ok = unsafe {
          CreateProcessW(
            application_name.as_ptr(),
            cmdline.as_mut_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            FALSE,
            flags,
            env_ptr,
            std::ptr::null(),
            std::ptr::addr_of_mut!(startup_ex).cast::<STARTUPINFOW>(),
            &mut pi,
          )
        };
        if ok == 0 {
          let err = io::Error::last_os_error();
          if mitigation_policy_attribute_unsupported(&err) {
            log_sandbox_debug(&format!(
              "windows sandbox: CreateProcessW rejected mitigation policy attribute ({err}); retrying without mitigations"
            ));
            let mut startup: STARTUPINFOW = unsafe { std::mem::zeroed() };
            startup.cb = std::mem::size_of::<STARTUPINFOW>() as u32;
            let mut pi: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };
            let mut cmdline_fallback = build_command_line(exe, args);
            let ok = unsafe {
              CreateProcessW(
                application_name.as_ptr(),
                cmdline_fallback.as_mut_ptr(),
                std::ptr::null(),
                std::ptr::null(),
                FALSE,
                flags & !EXTENDED_STARTUPINFO_PRESENT,
                env_ptr,
                std::ptr::null(),
                &mut startup,
                &mut pi,
              )
            };
            if ok == 0 {
              return Err(io::Error::last_os_error());
            }
            return Ok(pi);
          }
          return Err(err);
        }
        Ok(pi)
      };

      spawn_with_optional_breakaway(parent_in_job, &mut create_process, base_flags)?
    } else {
      let mut startup: STARTUPINFOW = unsafe { std::mem::zeroed() };
      startup.cb = std::mem::size_of::<STARTUPINFOW>() as u32;

      let mut create_process = |flags: u32| -> io::Result<PROCESS_INFORMATION> {
        let mut cmdline = build_command_line(exe, args);
        let mut pi: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };
        let ok = unsafe {
          CreateProcessW(
            application_name.as_ptr(),
            cmdline.as_mut_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            FALSE,
            flags,
            env_ptr,
            std::ptr::null(),
            &mut startup,
            &mut pi,
          )
        };
        if ok == 0 {
          return Err(io::Error::last_os_error());
        }
        Ok(pi)
      };

      spawn_with_optional_breakaway(
        parent_in_job,
        &mut create_process,
        CREATE_SUSPENDED | env_flags,
      )?
    }
  } else {
    let mut attrs_without_mitigations = AttributeList::new(1)?;
    attrs_without_mitigations.update_raw(
      PROC_THREAD_ATTRIBUTE_HANDLE_LIST,
      handles.as_mut_ptr().cast(),
      handles.len() * std::mem::size_of::<HANDLE>(),
    )?;

    let attrs_with_mitigations = if mitigation_policy_value != 0 {
      let mut attrs = AttributeList::new(2)?;
      attrs.update_raw(
        PROC_THREAD_ATTRIBUTE_HANDLE_LIST,
        handles.as_mut_ptr().cast(),
        handles.len() * std::mem::size_of::<HANDLE>(),
      )?;
      if let Err(err) = attrs.update_raw(
        PROC_THREAD_ATTRIBUTE_MITIGATION_POLICY,
        std::ptr::addr_of_mut!(mitigation_policy_value).cast(),
        std::mem::size_of::<u64>(),
      ) {
        if mitigation_policy_attribute_unsupported(&err) {
          log_sandbox_debug(&format!(
            "windows sandbox: UpdateProcThreadAttribute rejected mitigation policy attribute ({err}); continuing without mitigations"
          ));
          None
        } else {
          return Err(err);
        }
      } else {
        Some(attrs)
      }
    } else {
      None
    };

    let base_flags = CREATE_SUSPENDED | EXTENDED_STARTUPINFO_PRESENT | env_flags;

    let mut create_process = |flags: u32| -> io::Result<PROCESS_INFORMATION> {
      let mut create_process_with_attrs =
        |attr_list: *mut PROC_THREAD_ATTRIBUTE_LIST| -> io::Result<PROCESS_INFORMATION> {
          let mut cmdline = build_command_line(exe, args);
          let mut startup_ex: STARTUPINFOEXW = unsafe { std::mem::zeroed() };
          startup_ex.StartupInfo.cb = std::mem::size_of::<STARTUPINFOEXW>() as u32;
          startup_ex.lpAttributeList = attr_list;

          let mut pi: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };
          let ok = unsafe {
            CreateProcessW(
              application_name.as_ptr(),
              cmdline.as_mut_ptr(),
              std::ptr::null(),
              std::ptr::null(),
              TRUE,
              flags,
              env_ptr,
              std::ptr::null(),
              std::ptr::addr_of_mut!(startup_ex).cast::<STARTUPINFOW>(),
              &mut pi,
            )
          };
          if ok == 0 {
            return Err(io::Error::last_os_error());
          }
          Ok(pi)
        };

      if let Some(attrs) = attrs_with_mitigations.as_ref() {
        match create_process_with_attrs(attrs.list) {
          Ok(pi) => Ok(pi),
          Err(err) => {
            if mitigation_policy_attribute_unsupported(&err) {
              log_sandbox_debug(&format!(
                "windows sandbox: CreateProcessW rejected mitigation policy attribute ({err}); retrying without mitigations"
              ));
              create_process_with_attrs(attrs_without_mitigations.list)
            } else {
              Err(err)
            }
          }
        }
      } else {
        create_process_with_attrs(attrs_without_mitigations.list)
      }
    };

    spawn_with_optional_breakaway(parent_in_job, &mut create_process, base_flags)?
  };

  finish_spawn(
    job,
    pi,
    WindowsSandboxLevel::None,
    None,
    parent_in_job,
    used_breakaway,
    allow_jobless,
  )
}

fn finish_spawn(
  job: JobObject,
  pi: PROCESS_INFORMATION,
  level: WindowsSandboxLevel,
  temp_dir: Option<TempDir>,
  parent_in_job: bool,
  used_breakaway: bool,
  allow_jobless: bool,
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

  let mut job = Some(job);
  let assign_result = match job.as_ref() {
    Some(job_ref) => job_ref.assign_process(h_process),
    None => Err(io::Error::new(
      io::ErrorKind::Other,
      "windows renderer sandbox missing JobObject handle",
    )),
  };
  if let Err(err) = assign_result {
    if allow_jobless {
      eprintln!(
        "warning: Windows sandbox failed to assign child process {pid} to JobObject ({err}); \
 job limits (kill-on-close + active process limit) are NOT enforced (parent_in_job={parent_in_job}, used_breakaway={used_breakaway}, level={level:?})"
      );
      job = None;
    } else {
      // Fail closed: a missing job means we cannot enforce kill-on-close or the process creation
      // limit, which are security-relevant guardrails for the renderer process.
      //
      // This is commonly caused by running inside a parent Job object that disallows nested jobs /
      // breakaway. Require an explicit opt-in before allowing a downgrade.
      let _ = unsafe { TerminateProcess(h_process, 1) };
      return Err(io::Error::new(
        err.kind(),
        format!(
          "windows renderer sandbox failed to assign child process {pid} to JobObject ({err}); \
 this likely indicates nested jobs are unsupported or disallowed by the parent job (parent_in_job={parent_in_job}, used_breakaway={used_breakaway}, level={level:?}). \
 Set {ENV_ALLOW_UNSANDBOXED_RENDERER}=1 to allow running without full job containment."
        ),
      ));
    }
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

// -----------------------------------------------------------------------------
// AppContainer working directory + temp directory helpers
// -----------------------------------------------------------------------------

type GetAppContainerFolderPathFn =
  unsafe extern "system" fn(appcontainer_sid: *mut c_void, path: *mut *mut u16) -> i32;

struct GetAppContainerFolderPathApi {
  // Keep `userenv.dll` loaded for the lifetime of the process.
  _userenv: isize,
  func: GetAppContainerFolderPathFn,
}

#[link(name = "kernel32")]
extern "system" {
  fn LoadLibraryExW(name: *const u16, hfile: *mut c_void, flags: u32) -> isize;
  fn GetProcAddress(module: isize, proc_name: *const i8) -> *mut c_void;
  fn FreeLibrary(module: isize) -> i32;
}

// Force DLL resolution from `%SystemRoot%\\System32` to avoid search-order hijacking.
//
// Value is stable ABI: https://learn.microsoft.com/en-us/windows/win32/api/libloaderapi/nf-libloaderapi-loadlibraryexw
const LOAD_LIBRARY_SEARCH_SYSTEM32: u32 = 0x0000_0800;

#[link(name = "ole32")]
extern "system" {
  fn CoTaskMemFree(pv: *mut c_void);
}

fn get_appcontainer_folder_path_api() -> Option<&'static GetAppContainerFolderPathApi> {
  static API: OnceLock<Option<GetAppContainerFolderPathApi>> = OnceLock::new();
  API
    .get_or_init(|| unsafe {
      let userenv = wide_from_str("userenv.dll");
      let module = LoadLibraryExW(userenv.as_ptr(), std::ptr::null_mut(), LOAD_LIBRARY_SEARCH_SYSTEM32);
      if module == 0 {
        return None;
      }
      let proc = GetProcAddress(module, b"GetAppContainerFolderPath\0".as_ptr() as *const i8);
      if proc.is_null() {
        let _ = FreeLibrary(module);
        return None;
      }
      let func: GetAppContainerFolderPathFn = std::mem::transmute(proc);
      Some(GetAppContainerFolderPathApi {
        _userenv: module,
        func,
      })
    })
    .as_ref()
}

fn resolve_appcontainer_working_dirs(appcontainer_sid: *mut c_void) -> (PathBuf, PathBuf) {
  if let Some(api) = get_appcontainer_folder_path_api() {
    let mut raw_path: *mut u16 = std::ptr::null_mut();
    // SAFETY: `raw_path` is an out param; `appcontainer_sid` comes from a derived AppContainer SID.
    let hr = unsafe { (api.func)(appcontainer_sid, &mut raw_path) };
    if hr >= 0 && !raw_path.is_null() {
      let app_dir = unsafe { take_and_free_cotaskmem_wstr(raw_path) };
      let temp_dir = app_dir.join("Temp");
      if std::fs::create_dir_all(&temp_dir).is_ok() {
        return (app_dir, temp_dir);
      }
    }
  }

  // `GetAppContainerFolderPath` unavailable or failed: fall back to conservative system dirs.
  let cwd = PathBuf::from(FALLBACK_APPCONTAINER_CWD);
  let temp = PathBuf::from(FALLBACK_APPCONTAINER_TEMP);
  let _ = std::fs::create_dir_all(&temp);
  (cwd, temp)
}

unsafe fn take_and_free_cotaskmem_wstr(ptr: *mut u16) -> PathBuf {
  let mut len = 0usize;
  while *ptr.add(len) != 0 {
    len += 1;
  }
  let slice = std::slice::from_raw_parts(ptr, len);
  let os = OsString::from_wide(slice);
  CoTaskMemFree(ptr.cast());
  PathBuf::from(os)
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

fn collect_allowlisted_environment(temp_dir: &Path) -> Vec<(String, OsString)> {
  let mut vars = Vec::new();

  for key in ["SystemRoot", "WINDIR", "ComSpec", "PATHEXT"] {
    if let Some(value) = std::env::var_os(key) {
      vars.push((key.to_string(), value));
    }
  }

  if let Some(value) = std::env::var_os("RUST_BACKTRACE") {
    vars.push(("RUST_BACKTRACE".to_string(), value));
  }

  // Allow the mitigation-policy escape hatch to propagate to the child so sandboxed subprocesses
  // (including tests) can make consistent decisions about whether mitigations are expected.
  if let Some(value) = std::env::var_os("FASTR_DISABLE_WIN_MITIGATIONS") {
    vars.push(("FASTR_DISABLE_WIN_MITIGATIONS".to_string(), value));
  }

  let temp = temp_dir.as_os_str().to_os_string();
  vars.push(("TMP".to_string(), temp.clone()));
  vars.push(("TEMP".to_string(), temp));

  // Unit tests for the Windows sandbox spawn path coordinate via `FASTR_TEST_*` environment
  // variables. When building the crate's unit tests (`cfg(test)`), allow those test-only markers
  // through so spawned child test processes can detect their role.
  if cfg!(test) {
    for (key, value) in std::env::vars_os() {
      let key_str = key.to_string_lossy();
      if key_str.starts_with("FASTR_TEST_") || key_str == "RUST_TEST_THREADS" {
        vars.push((key_str.to_string(), value));
      }
    }
  }

  vars
}

fn environment_block_from_vars(mut vars: Vec<(String, OsString)>) -> Vec<u16> {
  // Windows expects the environment block to be sorted by key name.
  vars.sort_by(|(ak, _), (bk, _)| ak.to_ascii_uppercase().cmp(&bk.to_ascii_uppercase()));

  let mut block: Vec<u16> = Vec::new();
  for (key, value) in vars {
    block.extend(key.encode_utf16());
    block.push('=' as u16);
    block.extend(value.encode_wide());
    block.push(0);
  }
  // Environment blocks are double-NUL terminated.
  block.push(0);
  block
}

fn build_renderer_environment_block_for_temp_dir(temp_dir: &Path) -> Option<Vec<u16>> {
  if should_inherit_renderer_environment() {
    return None;
  }
  Some(environment_block_from_vars(collect_allowlisted_environment(
    temp_dir,
  )))
}

fn build_renderer_environment_block() -> Option<Vec<u16>> {
  let temp_dir: PathBuf = std::env::temp_dir();
  build_renderer_environment_block_for_temp_dir(&temp_dir)
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
  use std::ffi::OsString;
  use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener, TcpStream};
  use std::sync::Mutex;
  use std::time::Duration;

  use windows_sys::Win32::Foundation::{
    GetHandleInformation, SetHandleInformation, ERROR_INSUFFICIENT_BUFFER, HANDLE_FLAG_INHERIT,
    WAIT_OBJECT_0, WAIT_TIMEOUT,
  };
  use windows_sys::Win32::Security::Authorization::ConvertSidToStringSidW;
  use windows_sys::Win32::Security::{
    GetTokenInformation, TokenCapabilities, TokenIsAppContainer, TOKEN_GROUPS,
    TOKEN_INFORMATION_CLASS, TOKEN_MANDATORY_LABEL, TOKEN_QUERY,
  };
  use windows_sys::Win32::System::Threading::{GetExitCodeProcess, WaitForSingleObject};

  static ENV_LOCK: Mutex<()> = Mutex::new(());
  const JOB_CHILD_ENV: &str = "FASTR_TEST_WINDOWS_JOB_CHILD";
  const JOB_CHILD_TEST_NAME: &str =
    concat!(module_path!(), "::sandbox_spawn_in_job_does_not_panic_child");

  const CHILD_ENV: &str = "FASTR_TEST_WINDOWS_SANDBOX_CHILD";
  const PORT_ENV: &str = "FASTR_TEST_WINDOWS_SANDBOX_PORT";
  const INTERNET_CLIENT_CAPABILITY_SID: &str = "S-1-15-3-1";
  const CHILD_TIMEOUT_MS: u32 = 60_000;

  struct EnvVarGuard {
    key: &'static str,
    prev: Option<OsString>,
  }

  impl EnvVarGuard {
    fn set(key: &'static str, value: impl Into<OsString>) -> Self {
      let prev = std::env::var_os(key);
      std::env::set_var(key, value.into());
      Self { key, prev }
    }

    fn remove(key: &'static str) -> Self {
      let prev = std::env::var_os(key);
      std::env::remove_var(key);
      Self { key, prev }
    }
  }

  impl Drop for EnvVarGuard {
    fn drop(&mut self) {
      match self.prev.take() {
        Some(value) => std::env::set_var(self.key, value),
        None => std::env::remove_var(self.key),
      }
    }
  }

  struct HandleInheritGuard {
    saved: Vec<(HANDLE, u32)>,
  }

  impl HandleInheritGuard {
    fn new(handles: &[HANDLE]) -> Self {
      let mut saved = Vec::with_capacity(handles.len());
      for handle in handles {
        if *handle == 0 {
          continue;
        }
        let mut flags: u32 = 0;
        let ok = unsafe { GetHandleInformation(*handle, &mut flags) };
        if ok == 0 {
          continue;
        }
        saved.push((*handle, flags));
        let _ = unsafe { SetHandleInformation(*handle, HANDLE_FLAG_INHERIT, HANDLE_FLAG_INHERIT) };
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
        let _ = unsafe { SetHandleInformation(handle, HANDLE_FLAG_INHERIT, inherit) };
      }
    }
  }

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

  /// Regression coverage for environments where the current process is already inside a Windows
  /// Job that disallows breakaway/nested jobs (common in CI/supervisors).
  #[test]
  fn sandbox_spawn_in_job_does_not_panic() {
    // Run the actual job-munging logic in a dedicated subprocess so we don't permanently place the
    // main test runner into a Job (processes cannot leave a Job once assigned).
    let exe = std::env::current_exe().expect("current_exe");
    let status = std::process::Command::new(exe)
      .env(JOB_CHILD_ENV, "1")
      .arg("--nocapture")
      .arg("--exact")
      .arg(JOB_CHILD_TEST_NAME)
      .status()
      .expect("spawn child test process");
    assert!(status.success(), "child test process failed: {status}");
  }

  #[test]
  fn sandbox_spawn_in_job_does_not_panic_child() {
    if std::env::var_os(JOB_CHILD_ENV).is_none() {
      return;
    }

    // Ensure the process is inside a Job. If we're already in a job (CI), this is already true.
    // Otherwise, create a new job and assign ourselves to it so that the sandbox spawn code takes
    // the "parent already in job" path.
    let mut _job_guard: Option<OwnedHandle> = None;
    let in_job = current_process_in_job().unwrap_or(false);
    if !in_job {
      let job_handle = unsafe { CreateJobObjectW(std::ptr::null_mut(), std::ptr::null()) };
      if job_handle == 0 {
        eprintln!(
          "warning: CreateJobObjectW failed in Windows job regression test: {}",
          io::Error::last_os_error()
        );
        return;
      }
      let owned = unsafe { OwnedHandle::from_raw_handle(job_handle as RawHandle) };
      if let Err(err) = win32_bool(unsafe { AssignProcessToJobObject(job_handle, GetCurrentProcess()) }) {
        eprintln!(
          "warning: AssignProcessToJobObject(self) failed in Windows job regression test ({err}); skipping"
        );
        return;
      }
      _job_guard = Some(owned);
      assert!(
        current_process_in_job().unwrap_or(false),
        "expected IsProcessInJob to be true after assigning self to a Job"
      );
    }

    // Force the spawn path to use the "unsandboxed" mode so we can execute a well-known binary
    // (cmd.exe) without AppContainer policy interfering with the test.
    std::env::set_var(ENV_DISABLE_RENDERER_SANDBOX, "1");

    let cmd_exe = std::env::var_os("COMSPEC")
      .map(PathBuf::from)
      .or_else(|| {
        std::env::var_os("SystemRoot")
          .map(PathBuf::from)
          .map(|root| root.join("System32").join("cmd.exe"))
      })
      .unwrap_or_else(|| PathBuf::from("cmd.exe"));

    let args: Vec<OsString> = vec!["/C".into(), "exit".into(), "0".into()];
    let child = spawn_sandboxed(&cmd_exe, &args, &[]).expect("spawn_sandboxed");

    use windows_sys::Win32::Foundation::{WAIT_OBJECT_0, WAIT_TIMEOUT};
    use windows_sys::Win32::System::Threading::{GetExitCodeProcess, WaitForSingleObject};

    let wait_rc = unsafe { WaitForSingleObject(child.process.as_raw_handle() as HANDLE, 10_000) };
    match wait_rc {
      WAIT_OBJECT_0 => {}
      WAIT_TIMEOUT => {
        let _ = unsafe { TerminateProcess(child.process.as_raw_handle() as HANDLE, 1) };
        panic!("timed out waiting for sandbox child process to exit");
      }
      other => panic!("WaitForSingleObject returned {other}"),
    }

    let mut exit_code: u32 = 0;
    let ok = unsafe { GetExitCodeProcess(child.process.as_raw_handle() as HANDLE, &mut exit_code) };
    assert_ne!(ok, 0, "GetExitCodeProcess failed");
    assert_eq!(exit_code, 0, "sandbox child exited with code {exit_code}");
  }

  #[test]
  fn spawn_sandboxed_child_has_expected_token_state() {
    let is_child = std::env::var_os(CHILD_ENV).is_some();
    if is_child {
      let port: u16 = std::env::var(PORT_ENV)
        .expect("child process missing sandbox port env var")
        .parse()
        .expect("parse sandbox port env var");

      let token = query_current_process_token_state().expect("query current process token state");
      if token.is_app_container {
        assert!(
          !token.has_internet_client_capability(),
          "expected AppContainer token to NOT have internetClient capability ({INTERNET_CLIENT_CAPABILITY_SID}); token={token:?}"
        );

        if port != 0 {
          let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port);
          let connect = TcpStream::connect_timeout(&addr, Duration::from_secs(2));
          assert!(
            connect.is_err(),
            "expected AppContainer sandbox child to be unable to connect to localhost; token={token:?}, connect={connect:?}"
          );
        } else {
          eprintln!(
            "skipping localhost connect assertion in sandbox child: parent could not bind localhost"
          );
        }
        return;
      }
 
      panic!(
        "expected sandbox child to run in an AppContainer token (no silent fallback); token={token:?}"
      );
    }

    let _guard = ENV_LOCK.lock().unwrap();

    // Ensure debug escape hatches do not disable the sandbox for this test.
    let _disable_guard = EnvVarGuard::remove(ENV_DISABLE_RENDERER_SANDBOX);
    let _windows_guard = EnvVarGuard::remove(ENV_WINDOWS_RENDERER_SANDBOX);
    let _allow_guard = EnvVarGuard::remove(ENV_ALLOW_UNSANDBOXED_RENDERER);
    let _inherit_guard = EnvVarGuard::remove(ENV_INHERIT_RENDERER_ENV);

    let support = win_sandbox::SandboxSupport::detect();
    if support != win_sandbox::SandboxSupport::Full {
      eprintln!(
        "skipping Windows sandbox spawn token-state test: Windows sandbox is unavailable ({support})"
      );
      return;
    }

    // Some hardened Windows environments expose AppContainer APIs but deny creating/using profiles
    // (for example via group policy). In that case, spawning an AppContainer child will fail; skip
    // this unit test with a clear message instead of failing the entire suite.
    fn hresult_from_win32_code(hresult: u32) -> Option<u32> {
      const FACILITY_WIN32_MASK: u32 = 0xFFFF_0000;
      const FACILITY_WIN32_PREFIX: u32 = 0x8007_0000;
      if (hresult & FACILITY_WIN32_MASK) == FACILITY_WIN32_PREFIX {
        Some(hresult & 0xFFFF)
      } else {
        None
      }
    }

    fn win32_code_from_error(err: &win_sandbox::WinSandboxError) -> Option<u32> {
      match err {
        win_sandbox::WinSandboxError::Win32 { code, .. } => Some(*code),
        win_sandbox::WinSandboxError::HResult { hresult, .. } => {
          hresult_from_win32_code(*hresult)
        }
        _ => None,
      }
    }

    fn should_skip_appcontainer_profile_error(err: &win_sandbox::WinSandboxError) -> bool {
      const ERROR_ACCESS_DENIED: u32 = 5;
      const ERROR_ACCESS_DISABLED_BY_POLICY: u32 = 1260;
      const ERROR_PROC_NOT_FOUND: u32 = 127;
      const ERROR_NOT_SUPPORTED: u32 = 50;
      const ERROR_PRIVILEGE_NOT_HELD: u32 = 1314;

      match win32_code_from_error(err) {
        Some(code) => matches!(
          code,
          ERROR_ACCESS_DENIED
            | ERROR_ACCESS_DISABLED_BY_POLICY
            | ERROR_PRIVILEGE_NOT_HELD
            | ERROR_NOT_SUPPORTED
            | ERROR_PROC_NOT_FOUND
        ),
        None => false,
      }
    }

    match win_sandbox::AppContainerProfile::ensure(
      "FastRender.Renderer",
      "FastRender Renderer",
      "FastRender renderer AppContainer profile",
    ) {
      Ok(profile) => {
        if !profile.is_enabled() {
          eprintln!(
            "skipping Windows sandbox spawn token-state test: AppContainer profile is disabled"
          );
          return;
        }
      }
      Err(err) if should_skip_appcontainer_profile_error(&err) => {
        eprintln!(
          "skipping Windows sandbox spawn token-state test: AppContainer profile could not be ensured ({err})"
        );
        return;
      }
      Err(err) => panic!(
        "Windows sandbox spawn token-state test: AppContainer profile ensure failed unexpectedly: {err}"
      ),
    }

    let mut listener: Option<TcpListener> = None;
    let port = match TcpListener::bind(("127.0.0.1", 0)) {
      Ok(bound) => {
        let port = bound
          .local_addr()
          .expect("listener local addr")
          .port()
          .to_string();
        listener = Some(bound);
        port
      }
      Err(err)
        if matches!(
          err.kind(),
          io::ErrorKind::PermissionDenied | io::ErrorKind::AddrNotAvailable
        ) =>
      {
        eprintln!(
          "skipping localhost connect assertion in sandbox unit test: cannot bind localhost: {err}"
        );
        "0".to_string()
      }
      Err(err) => panic!("bind test TCP listener: {err}"),
    };

    let _child_guard = EnvVarGuard::set(CHILD_ENV, "1");
    let _port_guard = EnvVarGuard::set(PORT_ENV, port);
    // Reduce libtest's internal thread pool so the child is more deterministic.
    let _threads_guard = EnvVarGuard::set("RUST_TEST_THREADS", "1");

    let exe = std::env::current_exe().expect("current test exe path");
    let test_name = "sandbox::windows::tests::spawn_sandboxed_child_has_expected_token_state";
    let args = [
      OsString::from("--exact"),
      OsString::from(test_name),
      OsString::from("--nocapture"),
    ];

    let stdout = std::io::stdout().as_raw_handle();
    let stderr = std::io::stderr().as_raw_handle();
    let mut handles: Vec<RawHandle> = Vec::new();
    let mut to_make_inheritable: Vec<HANDLE> = Vec::new();
    if !stdout.is_null() {
      handles.push(stdout);
      to_make_inheritable.push(stdout as HANDLE);
    }
    if !stderr.is_null() && stderr != stdout {
      handles.push(stderr);
      to_make_inheritable.push(stderr as HANDLE);
    }
    let _inherit_guard = HandleInheritGuard::new(&to_make_inheritable);

    let child = spawn_sandboxed(&exe, &args, &handles).expect("spawn sandboxed child process");
    let exit_code = wait_for_exit_code(child.process.as_raw_handle() as HANDLE)
      .expect("wait for sandboxed child process");
    assert_eq!(
      exit_code, 0,
      "sandboxed child exited with non-zero code {exit_code} (level={:?})",
      child.level
    );

    drop(listener);
  }

  #[derive(Debug)]
  struct TokenState {
    is_app_container: bool,
    integrity_sid: String,
    integrity_rid: u32,
    capability_sids: Vec<String>,
  }

  impl TokenState {
    fn is_low_or_untrusted_integrity(&self) -> bool {
      matches!(self.integrity_rid, 0 | 4096)
    }

    fn has_internet_client_capability(&self) -> bool {
      self
        .capability_sids
        .iter()
        .any(|sid| sid.eq_ignore_ascii_case(INTERNET_CLIENT_CAPABILITY_SID))
    }
  }

  fn query_current_process_token_state() -> io::Result<TokenState> {
    let mut token: HANDLE = 0;
    win32_bool(unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) })?;
    if token == 0 {
      return Err(io::Error::new(
        io::ErrorKind::Other,
        "OpenProcessToken returned null token handle",
      ));
    }
    // SAFETY: We own the handle returned by OpenProcessToken and must close it.
    let token = unsafe { OwnedHandle::from_raw_handle(token as RawHandle) };

    let is_app_container = query_token_is_app_container(token.as_raw_handle() as HANDLE)?;
    let (integrity_sid, integrity_rid) = query_token_integrity_level(token.as_raw_handle() as HANDLE)?;
    let capability_sids = if is_app_container {
      query_token_capabilities(token.as_raw_handle() as HANDLE)?
    } else {
      Vec::new()
    };

    Ok(TokenState {
      is_app_container,
      integrity_sid,
      integrity_rid,
      capability_sids,
    })
  }

  fn query_token_is_app_container(token: HANDLE) -> io::Result<bool> {
    let mut value: u32 = 0;
    let mut returned: u32 = 0;
    let ok = unsafe {
      GetTokenInformation(
        token,
        TokenIsAppContainer as TOKEN_INFORMATION_CLASS,
        std::ptr::addr_of_mut!(value).cast(),
        std::mem::size_of::<u32>() as u32,
        std::ptr::addr_of_mut!(returned),
      )
    };
    win32_bool(ok)?;
    Ok(value != 0)
  }

  fn query_token_integrity_level(token: HANDLE) -> io::Result<(String, u32)> {
    let buf = get_token_information(token, TokenIntegrityLevel as TOKEN_INFORMATION_CLASS)?;
    if buf.len() < std::mem::size_of::<TOKEN_MANDATORY_LABEL>() {
      return Err(io::Error::new(
        io::ErrorKind::Other,
        format!(
          "TokenIntegrityLevel buffer too small ({} bytes)",
          buf.len()
        ),
      ));
    }

    // SAFETY: buffer is large enough to contain TOKEN_MANDATORY_LABEL.
    let label = unsafe { &*(buf.as_ptr().cast::<TOKEN_MANDATORY_LABEL>()) };
    let sid = label.Label.Sid;
    if sid.is_null() {
      return Err(io::Error::new(
        io::ErrorKind::Other,
        "TokenIntegrityLevel returned null SID",
      ));
    }
    let sid_string = sid_to_string(sid)?;
    let rid = sid_string
      .rsplit('-')
      .next()
      .and_then(|tail| tail.parse::<u32>().ok())
      .ok_or_else(|| {
        io::Error::new(
          io::ErrorKind::Other,
          format!("unexpected integrity SID format: {sid_string}"),
        )
      })?;
    Ok((sid_string, rid))
  }

  fn query_token_capabilities(token: HANDLE) -> io::Result<Vec<String>> {
    let buf = get_token_information(token, TokenCapabilities as TOKEN_INFORMATION_CLASS)?;
    if buf.is_empty() {
      return Ok(Vec::new());
    }

    if buf.len() < std::mem::size_of::<TOKEN_GROUPS>() {
      return Err(io::Error::new(
        io::ErrorKind::Other,
        format!("TokenCapabilities buffer too small ({} bytes)", buf.len()),
      ));
    }

    // SAFETY: buffer is large enough for TOKEN_GROUPS header.
    let groups = unsafe { &*(buf.as_ptr().cast::<TOKEN_GROUPS>()) };
    let count = groups.GroupCount as usize;
    let first = groups.Groups.as_ptr();
    let mut out = Vec::new();
    for idx in 0..count {
      // SAFETY: `idx < count` and the buffer returned by GetTokenInformation is sized to hold the
      // full array.
      let entry = unsafe { &*first.add(idx) };
      if entry.Sid.is_null() {
        continue;
      }
      out.push(sid_to_string(entry.Sid)?);
    }
    Ok(out)
  }

  fn get_token_information(token: HANDLE, class: TOKEN_INFORMATION_CLASS) -> io::Result<Vec<u8>> {
    let mut needed: u32 = 0;
    let ok = unsafe {
      GetTokenInformation(
        token,
        class,
        std::ptr::null_mut(),
        0,
        std::ptr::addr_of_mut!(needed),
      )
    };
    if ok != 0 {
      // Unexpected but possible for fixed-size info classes; treat as success with empty buffer.
      return Ok(Vec::new());
    }

    let err = io::Error::last_os_error();
    if err.raw_os_error() != Some(ERROR_INSUFFICIENT_BUFFER as i32) {
      return Err(err);
    }
    if needed == 0 {
      return Err(io::Error::new(
        io::ErrorKind::Other,
        "GetTokenInformation returned ERROR_INSUFFICIENT_BUFFER but length was 0",
      ));
    }

    let mut buf = vec![0u8; needed as usize];
    let ok = unsafe {
      GetTokenInformation(
        token,
        class,
        buf.as_mut_ptr().cast(),
        needed,
        std::ptr::addr_of_mut!(needed),
      )
    };
    win32_bool(ok)?;
    buf.truncate(needed as usize);
    Ok(buf)
  }

  fn sid_to_string(sid: *mut std::ffi::c_void) -> io::Result<String> {
    let mut wide: *mut u16 = std::ptr::null_mut();
    let ok = unsafe { ConvertSidToStringSidW(sid, std::ptr::addr_of_mut!(wide)) };
    win32_bool(ok)?;
    if wide.is_null() {
      return Err(io::Error::new(
        io::ErrorKind::Other,
        "ConvertSidToStringSidW succeeded but returned null pointer",
      ));
    }

    // SAFETY: `wide` is NUL-terminated per API contract.
    let mut len = 0usize;
    unsafe {
      while *wide.add(len) != 0 {
        len += 1;
      }
      let slice = std::slice::from_raw_parts(wide, len);
      let s = String::from_utf16_lossy(slice);
      LocalFree(wide as _);
      Ok(s)
    }
  }

  fn wait_for_exit_code(process: HANDLE) -> io::Result<u32> {
    let rc = unsafe { WaitForSingleObject(process, CHILD_TIMEOUT_MS) };
    match rc {
      WAIT_OBJECT_0 => {}
      WAIT_TIMEOUT => {
        let _ = unsafe { TerminateProcess(process, 1) };
        return Err(io::Error::new(
          io::ErrorKind::TimedOut,
          format!("sandboxed child timed out after {CHILD_TIMEOUT_MS}ms (terminated)"),
        ));
      }
      other => {
        return Err(io::Error::new(
          io::ErrorKind::Other,
          format!(
            "WaitForSingleObject failed (code={other}): {}",
            io::Error::last_os_error()
          ),
        ));
      }
    }

    let mut exit_code: u32 = 0;
    let ok = unsafe { GetExitCodeProcess(process, std::ptr::addr_of_mut!(exit_code)) };
    win32_bool(ok)?;
    Ok(exit_code)
  }
}
