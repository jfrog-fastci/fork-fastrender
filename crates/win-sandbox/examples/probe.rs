//! Windows sandbox probe tool.
//!
//! This example is intended as a lightweight debugging / repro utility when the Windows renderer
//! sandbox regresses. It spawns *itself* under the production sandbox launcher and prints observed
//! sandbox state from inside the child process.
//!
//! Usage:
//!
//! ```text
//! bash scripts/cargo_agent.sh run -p win-sandbox --example probe -- [--read <PATH>] [--connect <IP:PORT>] [--connect-localhost] [--no-aap-hardening]
//! ```
//!
//! Notes:
//! - `--connect-localhost` binds an ephemeral port on `127.0.0.1` in the parent and asks the child
//!   to connect to it. Under a no-capabilities AppContainer this should fail with `WSAEACCES`
//!   (10013), providing a deterministic "network is blocked" signal without requiring internet
//!   access.

#[cfg(not(windows))]
fn main() {
  eprintln!("win-sandbox example `probe` is only supported on Windows.");
  std::process::exit(2);
}

#[cfg(windows)]
fn main() {
  windows::main();
}

#[cfg(windows)]
mod windows {
  use std::ffi::c_void;
  use std::ffi::{OsStr, OsString};
  use std::fs;
  use std::io;
  use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream};
  use std::os::windows::ffi::OsStrExt;
  use std::os::windows::io::{AsRawHandle, RawHandle};
  use std::os::windows::io::{FromRawHandle, OwnedHandle};
  use std::path::Path;
  use std::path::PathBuf;
  use std::time::Duration;
  use std::time::{SystemTime, UNIX_EPOCH};

  use windows_sys::Win32::Foundation::{
    CloseHandle, GetLastError, SetHandleInformation, ERROR_ACCESS_DENIED, ERROR_NOT_SUPPORTED,
    FALSE, HANDLE_FLAG_INHERIT, INVALID_HANDLE_VALUE, TRUE,
  };
  use windows_sys::Win32::Security::{
    GetSidSubAuthority, GetSidSubAuthorityCount, GetTokenInformation, TokenCapabilities,
    TokenIntegrityLevel, TokenIsAppContainer, TokenGroups, DACL_SECURITY_INFORMATION, PSID,
    SECURITY_CAPABILITIES, SID_AND_ATTRIBUTES, TOKEN_GROUPS, TOKEN_MANDATORY_LABEL, TOKEN_QUERY,
  };
  use windows_sys::Win32::Security::Authorization::{
    ConvertSidToStringSidW, GetNamedSecurityInfoW, SetEntriesInAclW, SetNamedSecurityInfoW,
    EXPLICIT_ACCESS_W, GRANT_ACCESS, NO_MULTIPLE_TRUSTEE, SE_FILE_OBJECT, TRUSTEE_IS_SID,
    TRUSTEE_IS_UNKNOWN, TRUSTEE_W,
  };
  use windows_sys::Win32::Storage::FileSystem::{FILE_GENERIC_EXECUTE, FILE_GENERIC_READ};
  use windows_sys::Win32::System::Console::{
    GetStdHandle, STD_ERROR_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE,
  };
  use windows_sys::Win32::System::JobObjects::IsProcessInJob;
  use windows_sys::Win32::System::Threading::{
    CreateProcessAsUserW, CreateProcessW, DeleteProcThreadAttributeList, GetCurrentProcess,
    GetExitCodeProcess, InitializeProcThreadAttributeList,
    OpenProcessToken,
    ProcessDynamicCodePolicy, ProcessExtensionPointDisablePolicy, ProcessImageLoadPolicy,
    ProcessStrictHandleCheckPolicy, ProcessSystemCallDisablePolicy, ResumeThread, TerminateProcess,
    UpdateProcThreadAttribute, WaitForSingleObject, CREATE_BREAKAWAY_FROM_JOB, CREATE_SUSPENDED,
    EXTENDED_STARTUPINFO_PRESENT, PROCESS_INFORMATION, PROCESS_MITIGATION_POLICY,
    STARTUPINFOEXW, STARTUPINFOW, LPPROC_THREAD_ATTRIBUTE_LIST,
  };

  use win_sandbox::{mitigations, AppContainerProfile, Job, RestrictedToken, WinSandboxError};

  // WaitForSingleObject return codes.
  const WAIT_OBJECT_0: u32 = 0;
  const WAIT_TIMEOUT: u32 = 0x0000_0102;

  // We keep the process creation probe simple: spawn the child, wait for it, and propagate its
  // exit code.
  const DEFAULT_TIMEOUT_MS: u32 = 30_000;

  #[derive(Debug, Default)]
  struct Args {
    child: bool,
    read_path: Option<PathBuf>,
    connect: Option<OsString>,
    connect_localhost: bool,
    aap_hardened: bool,
    timeout_ms: u32,
  }

  // -----------------------------------------------------------------------------
  // RendererSandbox spawner (parent-side)
  // -----------------------------------------------------------------------------

  /// The environment variable used by the production Windows renderer sandbox as a debug escape
  /// hatch.
  const ENV_DISABLE_RENDERER_SANDBOX: &str = "FASTR_DISABLE_RENDERER_SANDBOX";

  /// Legacy/alternate spelling for disabling the Windows renderer sandbox.
  const ENV_WINDOWS_RENDERER_SANDBOX: &str = "FASTR_WINDOWS_RENDERER_SANDBOX";

  /// Best-effort job memory limit (in MiB) applied to the renderer Job object.
  const JOB_MEM_LIMIT_ENV: &str = "FASTR_RENDERER_JOB_MEM_LIMIT_MB";

  /// Proc thread attribute value for `PROC_THREAD_ATTRIBUTE_MITIGATION_POLICY`.
  ///
  /// This matches `ProcThreadAttributeValue(7, FALSE, TRUE, FALSE)` from the Windows SDK headers.
  const PROC_THREAD_ATTRIBUTE_MITIGATION_POLICY: usize = 0x0002_0007;
  // `PROC_THREAD_ATTRIBUTE_*` values are stable ABI constants from winbase.h:
  //   ProcThreadAttributeValue(Number, Thread, Input, Additive)
  const PROC_THREAD_ATTRIBUTE_HANDLE_LIST: usize = 0x0002_0002;
  const PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES: usize = 0x0002_0009;
  // ProcThreadAttributeValue(15, FALSE, TRUE, FALSE) → 0x0002_000F.
  const PROC_THREAD_ATTRIBUTE_ALL_APPLICATION_PACKAGES_POLICY: usize = 0x0002_000F;
  // Value for `PROC_THREAD_ATTRIBUTE_ALL_APPLICATION_PACKAGES_POLICY` (winbase.h).
  const PROCESS_CREATION_ALL_APPLICATION_PACKAGES_POLICY_BLOCK: u32 = 1;

  #[derive(Debug, Clone, Copy, PartialEq, Eq)]
  enum SandboxMode {
    AppContainer,
    RestrictedToken,
    Unsandboxed,
  }

  #[derive(Debug)]
  struct SpawnedChild {
    process: OwnedHandle,
    pid: u32,
    mode: SandboxMode,
    // Keep the job object alive for the lifetime of the process so kill-on-close/process limits
    // remain enforced.
    _job: Job,
    job_assigned: bool,
    used_breakaway_from_job: bool,
    // Keep relocated image directory alive while the process runs.
    _relocated_image_dir: Option<TempDir>,
  }

  struct RendererSandbox;

  impl RendererSandbox {
    fn spawn(
      exe: &Path,
      args: &[OsString],
      inherit_handles: &[RawHandle],
      all_application_packages_hardened: bool,
    ) -> io::Result<SpawnedChild> {
      let mitigation_policy = if std::env::var_os("FASTR_DISABLE_WIN_MITIGATIONS").is_some() {
        0
      } else {
        mitigations::renderer_mitigation_policy()
      };

      let parent_in_job = current_process_in_job().unwrap_or(false);

      if renderer_sandbox_disabled_via_env() {
        eprintln!("warning: renderer sandbox disabled via env; spawning unsandboxed");
        return spawn_process_unsandboxed(
          exe,
          args,
          inherit_handles,
          mitigation_policy,
          parent_in_job,
        );
      }

      match spawn_process_appcontainer(
        exe,
        args,
        inherit_handles,
        mitigation_policy,
        parent_in_job,
        all_application_packages_hardened,
      ) {
        Ok(child) => return Ok(child),
        Err(err) => {
          eprintln!("warning: AppContainer spawn failed: {err}; falling back to restricted token");
        }
      }

      match spawn_process_restricted_token(
        exe,
        args,
        inherit_handles,
        mitigation_policy,
        parent_in_job,
      ) {
        Ok(child) => Ok(child),
        Err(err) => {
          eprintln!("warning: restricted-token spawn failed: {err}; spawning unsandboxed");
          spawn_process_unsandboxed(
            exe,
            args,
            inherit_handles,
            mitigation_policy,
            parent_in_job,
          )
        }
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

  fn job_memory_limit_bytes_from_env() -> io::Result<Option<usize>> {
    match std::env::var(JOB_MEM_LIMIT_ENV) {
      Ok(raw) => {
        let trimmed = raw.trim();
        if trimmed.is_empty() || trimmed == "0" {
          return Ok(None);
        }

        // Allow `_` separators for readability (e.g. `1_024`).
        let normalized: String = trimmed.chars().filter(|c| *c != '_').collect();
        let mb: u64 = normalized.parse().map_err(|_| {
          io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("invalid environment variable `{JOB_MEM_LIMIT_ENV}`: `{raw}`"),
          )
        })?;
        if mb == 0 {
          return Ok(None);
        }
        let bytes = mb.saturating_mul(1024 * 1024);
        Ok(Some(bytes.min(usize::MAX as u64) as usize))
      }
      Err(std::env::VarError::NotPresent) => Ok(None),
      Err(std::env::VarError::NotUnicode(_)) => Err(io::Error::new(
        io::ErrorKind::InvalidInput,
        format!("invalid environment variable `{JOB_MEM_LIMIT_ENV}`: <non-unicode>"),
      )),
    }
  }

  fn spawn_process_appcontainer(
    exe: &Path,
    args: &[OsString],
    inherit_handles: &[RawHandle],
    mitigation_policy: u64,
    parent_in_job: bool,
    all_application_packages_hardened: bool,
  ) -> io::Result<SpawnedChild> {
    let profile = AppContainerProfile::ensure(
      "FastRender.Renderer",
      "FastRender.Renderer",
      "FastRender renderer sandbox",
    )
    .map_err(win_err)?;

    let job = Job::new(None).map_err(win_err)?;
    job
      .set_renderer_limits(job_memory_limit_bytes_from_env()?)
      .map_err(win_err)?;

    // Always relocate the image to a temp dir that we can ACL for the AppContainer SID; this avoids
    // common `ERROR_ACCESS_DENIED` failures when running from a dev checkout.
    let appcontainer_sid = profile.sid();
    let (temp_dir, relocated_exe) = relocate_exe_for_appcontainer(exe, appcontainer_sid.as_ptr())?;

    let current_dir_w = wide_from_os(temp_dir.path.as_os_str());
    let current_dir_ptr = current_dir_w.as_ptr();

    let (pi, used_breakaway) = spawn_with_optional_breakaway(parent_in_job, |flags| {
      spawn_with_attributes(
        Some(appcontainer_sid.as_ptr()),
        None,
        &relocated_exe,
        args,
        inherit_handles,
        mitigation_policy,
        all_application_packages_hardened,
        flags,
        Some(current_dir_ptr),
      )
    })?;

    let child = finish_spawn(pi, SandboxMode::AppContainer, job, used_breakaway)?;
    Ok(SpawnedChild {
      _relocated_image_dir: Some(temp_dir),
      ..child
    })
  }

  fn spawn_process_restricted_token(
    exe: &Path,
    args: &[OsString],
    inherit_handles: &[RawHandle],
    mitigation_policy: u64,
    parent_in_job: bool,
  ) -> io::Result<SpawnedChild> {
    let token = RestrictedToken::for_current_process_low_integrity().map_err(win_err)?;

    let job = Job::new(None).map_err(win_err)?;
    job
      .set_renderer_limits(job_memory_limit_bytes_from_env()?)
      .map_err(win_err)?;

    // If `lpCurrentDirectory` is NULL, Windows inherits the parent's current directory. For a
    // low-integrity restricted token that directory may be inaccessible, causing
    // `CreateProcessAsUserW` to fail with `ERROR_ACCESS_DENIED`.
    //
    // Prefer the executable's parent directory; fall back to a conservative system directory.
    let cwd = exe
      .parent()
      .unwrap_or_else(|| Path::new(r"C:\Windows\System32"));
    let cwd_w = wide_from_os(cwd.as_os_str());
    let cwd_ptr = cwd_w.as_ptr();

    let (pi, used_breakaway) = spawn_with_optional_breakaway(parent_in_job, |flags| {
      spawn_with_attributes(
        None,
        Some(token.handle()),
        exe,
        args,
        inherit_handles,
        mitigation_policy,
        false,
        flags,
        Some(cwd_ptr),
      )
    })?;

    finish_spawn(pi, SandboxMode::RestrictedToken, job, used_breakaway)
  }

  fn spawn_process_unsandboxed(
    exe: &Path,
    args: &[OsString],
    inherit_handles: &[RawHandle],
    mitigation_policy: u64,
    parent_in_job: bool,
  ) -> io::Result<SpawnedChild> {
    // Best-effort: still apply mitigations + job limits even when the sandbox is disabled/fails.
    let job = Job::new(None).map_err(win_err)?;
    job
      .set_renderer_limits(job_memory_limit_bytes_from_env()?)
      .map_err(win_err)?;

    let (pi, used_breakaway) = spawn_with_optional_breakaway(parent_in_job, |flags| {
      spawn_with_attributes(
        None,
        None,
        exe,
        args,
        inherit_handles,
        mitigation_policy,
        false,
        flags,
        None,
      )
    })?;

    finish_spawn(pi, SandboxMode::Unsandboxed, job, used_breakaway)
  }

  fn spawn_with_optional_breakaway<F>(
    parent_in_job: bool,
    mut create: F,
  ) -> io::Result<(PROCESS_INFORMATION, bool)>
  where
    F: FnMut(u32) -> io::Result<PROCESS_INFORMATION>,
  {
    let base_flags = CREATE_SUSPENDED;
    if !parent_in_job {
      return create(base_flags).map(|pi| (pi, false));
    }

    match create(base_flags | CREATE_BREAKAWAY_FROM_JOB) {
      Ok(pi) => Ok((pi, true)),
      Err(err) => {
        if err.raw_os_error() != Some(ERROR_ACCESS_DENIED as i32) {
          return Err(err);
        }
        eprintln!(
          "warning: CreateProcess* with CREATE_BREAKAWAY_FROM_JOB returned ERROR_ACCESS_DENIED; retrying without breakaway"
        );
        create(base_flags).map(|pi| (pi, false))
      }
    }
  }

  fn should_fallback_without_mitigations(err: &io::Error) -> bool {
    const ERROR_INVALID_PARAMETER: i32 = 87;
    let not_supported = ERROR_NOT_SUPPORTED as i32;
    matches!(err.raw_os_error(), Some(ERROR_INVALID_PARAMETER)) || err.raw_os_error() == Some(not_supported)
  }

  fn spawn_with_attributes(
    appcontainer_sid: Option<PSID>,
    restricted_token: Option<windows_sys::Win32::Foundation::HANDLE>,
    exe: &Path,
    args: &[OsString],
    inherit_handles: &[RawHandle],
    mitigation_policy: u64,
    all_application_packages_hardened: bool,
    creation_flags: u32,
    current_dir: Option<*const u16>,
  ) -> io::Result<PROCESS_INFORMATION> {
    let want_aap = all_application_packages_hardened && appcontainer_sid.is_some();
    let want_mitigations = mitigation_policy != 0;

    // Try the strongest configuration first, then fall back when the host OS rejects particular
    // `STARTUPINFOEX` attributes (commonly `ERROR_INVALID_PARAMETER (87)` /
    // `ERROR_NOT_SUPPORTED (50)`).
    let mut attempts: Vec<(u64, bool, &'static str)> = Vec::new();
    if want_mitigations && want_aap {
      attempts.push((mitigation_policy, true, "mitigations + AAP hardening"));
      attempts.push((mitigation_policy, false, "mitigations (no AAP hardening)"));
      attempts.push((0, true, "AAP hardening (no mitigations)"));
      attempts.push((0, false, "no mitigations, no AAP hardening"));
    } else if want_mitigations {
      attempts.push((mitigation_policy, false, "mitigations"));
      attempts.push((0, false, "no mitigations"));
    } else if want_aap {
      attempts.push((0, true, "AAP hardening"));
      attempts.push((0, false, "no AAP hardening"));
    } else {
      attempts.push((0, false, "no startup attributes"));
    }

    let mut last_optional_err: Option<io::Error> = None;
    for (mitigations, include_aap, label) in attempts {
      match spawn_with_attributes_inner(
        appcontainer_sid,
        restricted_token,
        exe,
        args,
        inherit_handles,
        mitigations,
        include_aap,
        creation_flags,
        current_dir,
      ) {
        Ok(pi) => return Ok(pi),
        Err(err) if should_fallback_without_mitigations(&err) => {
          eprintln!(
            "warning: CreateProcess* rejected startup attributes ({label}): {err}; retrying with weaker attribute set"
          );
          last_optional_err = Some(err);
          continue;
        }
        Err(err) => return Err(err),
      }
    }
    Err(last_optional_err.unwrap_or_else(|| {
      io::Error::new(
        io::ErrorKind::Other,
        "CreateProcess* failed with unsupported startup attributes",
      )
    }))
  }

  fn spawn_with_attributes_inner(
    appcontainer_sid: Option<PSID>,
    restricted_token: Option<windows_sys::Win32::Foundation::HANDLE>,
    exe: &Path,
    args: &[OsString],
    inherit_handles: &[RawHandle],
    mitigation_policy: u64,
    include_aap: bool,
    creation_flags: u32,
    current_dir: Option<*const u16>,
  ) -> io::Result<PROCESS_INFORMATION> {
    let application_name = wide_from_os(exe.as_os_str());
    let mut command_line = build_command_line(exe, args);

    let handles: Vec<windows_sys::Win32::Foundation::HANDLE> = inherit_handles
      .iter()
      .copied()
      .map(|h| h as windows_sys::Win32::Foundation::HANDLE)
      .collect();
    let mut handles = handles;
    let inherit = if handles.is_empty() { FALSE } else { TRUE };

    let mut security_caps = SECURITY_CAPABILITIES {
      AppContainerSid: appcontainer_sid.unwrap_or(std::ptr::null_mut()),
      Capabilities: std::ptr::null_mut(),
      CapabilityCount: 0,
      Reserved: 0,
    };

    let mut all_packages_policy_value = PROCESS_CREATION_ALL_APPLICATION_PACKAGES_POLICY_BLOCK;
    let mut mitigation_policy_value = mitigation_policy;

    let mut attr_count = 0u32;
    if appcontainer_sid.is_some() {
      attr_count += 1;
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

    if attr_count == 0 {
      let mut startup: STARTUPINFOW = unsafe { std::mem::zeroed() };
      startup.cb = std::mem::size_of::<STARTUPINFOW>() as u32;

      let mut pi: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };
      let ok = unsafe {
        if let Some(token) = restricted_token {
          CreateProcessAsUserW(
            token,
            application_name.as_ptr(),
            command_line.as_mut_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            inherit,
            creation_flags,
            std::ptr::null(),
            current_dir.unwrap_or(std::ptr::null()),
            &mut startup,
            &mut pi,
          )
        } else {
          CreateProcessW(
            application_name.as_ptr(),
            command_line.as_mut_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            inherit,
            creation_flags,
            std::ptr::null(),
            current_dir.unwrap_or(std::ptr::null()),
            &mut startup,
            &mut pi,
          )
        }
      };
      if ok == 0 {
        return Err(io::Error::last_os_error());
      }
      return Ok(pi);
    }

    let mut startup_info_ex: STARTUPINFOEXW = unsafe { std::mem::zeroed() };
    startup_info_ex.StartupInfo.cb = std::mem::size_of::<STARTUPINFOEXW>() as u32;

    let mut attr_list = ProcThreadAttributeList::new(attr_count)?;
    if appcontainer_sid.is_some() {
      attr_list.update(
        PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES as usize,
        std::ptr::addr_of_mut!(security_caps).cast(),
        std::mem::size_of::<SECURITY_CAPABILITIES>(),
      )?;
      if include_aap {
        if let Err(err) = attr_list.update(
          PROC_THREAD_ATTRIBUTE_ALL_APPLICATION_PACKAGES_POLICY,
          std::ptr::addr_of_mut!(all_packages_policy_value).cast(),
          std::mem::size_of::<u32>(),
        ) {
          if should_fallback_without_mitigations(&err) {
            eprintln!(
              "warning: UpdateProcThreadAttribute(PROC_THREAD_ATTRIBUTE_ALL_APPLICATION_PACKAGES_POLICY) rejected by OS ({err}); continuing without AAP hardening"
            );
          } else {
            return Err(err);
          }
        }
      }
    }
    if !handles.is_empty() {
      attr_list.update(
        PROC_THREAD_ATTRIBUTE_HANDLE_LIST as usize,
        handles.as_mut_ptr().cast(),
        handles.len() * std::mem::size_of::<windows_sys::Win32::Foundation::HANDLE>(),
      )?;
    }
    if mitigation_policy != 0 {
      attr_list.update(
        PROC_THREAD_ATTRIBUTE_MITIGATION_POLICY,
        std::ptr::addr_of_mut!(mitigation_policy_value).cast(),
        std::mem::size_of::<u64>(),
      )?;
    }
    startup_info_ex.lpAttributeList = attr_list.ptr;

    let mut pi: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };
    let ok = unsafe {
      if let Some(token) = restricted_token {
        CreateProcessAsUserW(
          token,
          application_name.as_ptr(),
          command_line.as_mut_ptr(),
          std::ptr::null(),
          std::ptr::null(),
          inherit,
          creation_flags | EXTENDED_STARTUPINFO_PRESENT,
          std::ptr::null(),
          current_dir.unwrap_or(std::ptr::null()),
          std::ptr::addr_of_mut!(startup_info_ex).cast::<STARTUPINFOW>(),
          &mut pi,
        )
      } else {
        CreateProcessW(
          application_name.as_ptr(),
          command_line.as_mut_ptr(),
          std::ptr::null(),
          std::ptr::null(),
          inherit,
          creation_flags | EXTENDED_STARTUPINFO_PRESENT,
          std::ptr::null(),
          current_dir.unwrap_or(std::ptr::null()),
          std::ptr::addr_of_mut!(startup_info_ex).cast::<STARTUPINFOW>(),
          &mut pi,
        )
      }
    };
    drop(attr_list);
    if ok == 0 {
      return Err(io::Error::last_os_error());
    }
    Ok(pi)
  }

  fn finish_spawn(
    pi: PROCESS_INFORMATION,
    mode: SandboxMode,
    job: Job,
    used_breakaway_from_job: bool,
  ) -> io::Result<SpawnedChild> {
    // SAFETY: handles returned by CreateProcess*.
    let process = unsafe { OwnedHandle::from_raw_handle(pi.hProcess as RawHandle) };

    let job_assigned = match job.assign_process(&process) {
      Ok(()) => true,
      Err(err) => {
        match &err {
          WinSandboxError::Win32 { code, .. } if *code == ERROR_ACCESS_DENIED => {
            eprintln!(
              "warning: AssignProcessToJobObject returned ERROR_ACCESS_DENIED; continuing without job enforcement"
            );
          }
          _ => eprintln!("warning: AssignProcessToJobObject failed: {err}"),
        }
        false
      }
    };

    // Resume the main thread after job assignment.
    let resume = unsafe { ResumeThread(pi.hThread) };
    if resume == u32::MAX {
      unsafe {
        let _ = CloseHandle(pi.hThread);
      }
      return Err(io::Error::last_os_error());
    }
    unsafe {
      CloseHandle(pi.hThread);
    }

    Ok(SpawnedChild {
      process,
      pid: pi.dwProcessId,
      mode,
      _job: job,
      job_assigned,
      used_breakaway_from_job,
      _relocated_image_dir: None,
    })
  }

  fn win_err(err: WinSandboxError) -> io::Error {
    io::Error::new(io::ErrorKind::Other, err.to_string())
  }

  #[derive(Debug)]
  struct TempDir {
    path: PathBuf,
  }

  impl TempDir {
    fn new(prefix: &str) -> io::Result<Self> {
      let mut path = std::env::temp_dir();
      let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
      path.push(format!("{prefix}{}-{nanos}", std::process::id()));
      fs::create_dir(&path)?;
      Ok(Self { path })
    }
  }

  impl Drop for TempDir {
    fn drop(&mut self) {
      let _ = fs::remove_dir_all(&self.path);
    }
  }

  fn relocate_exe_for_appcontainer(exe: &Path, appcontainer_sid: PSID) -> io::Result<(TempDir, PathBuf)> {
    let temp_dir = TempDir::new("win-sandbox-probe-image-")?;
    let file_name = exe
      .file_name()
      .filter(|name| !name.is_empty())
      .unwrap_or_else(|| OsStr::new("probe.exe"));
    let dst = temp_dir.path.join(file_name);
    fs::copy(exe, &dst)?;

    // Best-effort grant directory access.
    let _ = grant_read_execute_acl(&temp_dir.path, appcontainer_sid);
    grant_read_execute_acl(&dst, appcontainer_sid)?;

    Ok((temp_dir, dst))
  }

  fn grant_read_execute_acl(path: &Path, sid: PSID) -> io::Result<()> {
    // `NO_INHERITANCE` from `accctrl.h` is defined as 0. `windows-sys` does not reliably export
    // this constant for all targets, so keep a local copy here.
    const NO_INHERITANCE: u32 = 0;

    let mut name = wide_from_os(path.as_os_str());

    let mut dacl: *mut windows_sys::Win32::Security::ACL = std::ptr::null_mut();
    let mut sd: *mut c_void = std::ptr::null_mut();

    // SAFETY: output pointers are writable.
    let status = unsafe {
      GetNamedSecurityInfoW(
        name.as_mut_ptr(),
        SE_FILE_OBJECT,
        DACL_SECURITY_INFORMATION,
        std::ptr::null_mut(),
        std::ptr::null_mut(),
        &mut dacl,
        std::ptr::null_mut(),
        &mut sd,
      )
    };
    if status != 0 {
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
    if status != 0 {
      unsafe {
        windows_sys::Win32::Foundation::LocalFree(sd as _);
      }
      return Err(io::Error::from_raw_os_error(status as i32));
    }

    let status = unsafe {
      SetNamedSecurityInfoW(
        name.as_mut_ptr(),
        SE_FILE_OBJECT,
        DACL_SECURITY_INFORMATION,
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
      return Err(io::Error::from_raw_os_error(status as i32));
    }
    Ok(())
  }

  fn wide_from_os(value: &OsStr) -> Vec<u16> {
    value.encode_wide().chain(Some(0)).collect()
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
        for _ in 0..(backslashes * 2 + 1) {
          cmd.push(b'\\' as u16);
        }
        cmd.push(b'"' as u16);
        backslashes = 0;
        continue;
      }

      for _ in 0..backslashes {
        cmd.push(b'\\' as u16);
      }
      backslashes = 0;
      cmd.push(ch);
    }

    for _ in 0..(backslashes * 2) {
      cmd.push(b'\\' as u16);
    }
    cmd.push(b'"' as u16);
  }

  fn build_command_line(program: &Path, args: &[OsString]) -> Vec<u16> {
    let mut cmd: Vec<u16> = Vec::new();
    append_arg_escaped(&mut cmd, program.as_os_str());
    for arg in args {
      cmd.push(b' ' as u16);
      append_arg_escaped(&mut cmd, arg.as_os_str());
    }
    cmd.push(0);
    cmd
  }

  struct ProcThreadAttributeList {
    // Keep the backing allocation alive for the lifetime of `ptr`.
    _buf: Vec<u64>,
    ptr: LPPROC_THREAD_ATTRIBUTE_LIST,
  }

  impl ProcThreadAttributeList {
    fn new(attribute_count: u32) -> io::Result<Self> {
      let mut size: usize = 0;
      let ok = unsafe { InitializeProcThreadAttributeList(std::ptr::null_mut(), attribute_count, 0, &mut size) };
      if ok != 0 {
        return Err(io::Error::new(
          io::ErrorKind::Other,
          "InitializeProcThreadAttributeList(size query) unexpectedly succeeded",
        ));
      }
      let err = unsafe { GetLastError() };
      const ERROR_INSUFFICIENT_BUFFER: u32 = 122;
      if err != ERROR_INSUFFICIENT_BUFFER {
        return Err(io::Error::from_raw_os_error(err as i32));
      }

      let mut buf: Vec<u64> = vec![0; (size + 7) / 8];
      let ptr = buf.as_mut_ptr() as LPPROC_THREAD_ATTRIBUTE_LIST;
      let ok = unsafe { InitializeProcThreadAttributeList(ptr, attribute_count, 0, &mut size) };
      if ok == 0 {
        return Err(io::Error::last_os_error());
      }
      Ok(Self { _buf: buf, ptr })
    }

    fn update(&mut self, attribute: usize, value: *mut c_void, size: usize) -> io::Result<()> {
      let ok = unsafe {
        UpdateProcThreadAttribute(
          self.ptr,
          0,
          attribute,
          value,
          size,
          std::ptr::null_mut(),
          std::ptr::null_mut(),
        )
      };
      if ok == 0 {
        return Err(io::Error::last_os_error());
      }
      Ok(())
    }
  }

  impl Drop for ProcThreadAttributeList {
    fn drop(&mut self) {
      unsafe {
        DeleteProcThreadAttributeList(self.ptr);
      }
    }
  }

  pub(super) fn main() {
    let args = match parse_args() {
      Ok(args) => args,
      Err(err) => {
        eprintln!("{err}\n");
        print_usage();
        std::process::exit(2);
      }
    };

    let exit_code = if args.child {
      run_child(&args)
    } else {
      run_parent(&args)
    };
    std::process::exit(exit_code);
  }

  fn parse_args() -> Result<Args, String> {
    let mut out = Args {
      timeout_ms: DEFAULT_TIMEOUT_MS,
      aap_hardened: true,
      ..Args::default()
    };

    let mut iter = std::env::args_os().skip(1);
    while let Some(arg) = iter.next() {
      if arg == OsStr::new("--child") {
        out.child = true;
      } else if arg == OsStr::new("--read") {
        let Some(value) = iter.next() else {
          return Err("missing value for --read".to_string());
        };
        out.read_path = Some(PathBuf::from(value));
      } else if arg == OsStr::new("--connect") {
        let Some(value) = iter.next() else {
          return Err("missing value for --connect".to_string());
        };
        out.connect = Some(value);
      } else if arg == OsStr::new("--connect-localhost") {
        out.connect_localhost = true;
      } else if arg == OsStr::new("--no-aap-hardening") {
        out.aap_hardened = false;
      } else if arg == OsStr::new("--timeout-ms") {
        let Some(value) = iter.next() else {
          return Err("missing value for --timeout-ms".to_string());
        };
        let value = value.to_string_lossy();
        out.timeout_ms = value
          .trim()
          .parse::<u32>()
          .map_err(|_| format!("invalid --timeout-ms value: {value}"))?;
      } else if arg == OsStr::new("--help") || arg == OsStr::new("-h") {
        print_usage();
        std::process::exit(0);
      } else {
        return Err(format!("unrecognized argument: {}", arg.to_string_lossy()));
      }
    }

    if out.connect_localhost && out.connect.is_some() {
      return Err("cannot combine --connect-localhost with --connect".to_string());
    }

    Ok(out)
  }

  fn print_usage() {
    eprintln!(
      "Usage:\n  # From the workspace root (wrapper-friendly):\n  bash scripts/cargo_agent.sh run -p win-sandbox --example probe -- [--read <PATH>] [--connect <IP:PORT>] [--connect-localhost] [--no-aap-hardening] [--timeout-ms <MS>]\n\n\
  # Or directly via cargo:\n  cargo run -p win-sandbox --example probe -- [--read <PATH>] [--connect <IP:PORT>] [--connect-localhost] [--no-aap-hardening] [--timeout-ms <MS>]\n\n\
Parent mode (default) spawns a sandboxed child.\nChild mode (--child) prints sandbox state and runs probes.\n"
    );
  }

  fn run_parent(args: &Args) -> i32 {
    println!("== win-sandbox probe (parent) ==");
    println!("pid: {}", std::process::id());

    if let Some(value) = std::env::var_os("FASTR_DISABLE_RENDERER_SANDBOX") {
      println!(
        "env: FASTR_DISABLE_RENDERER_SANDBOX={} (sandbox may be disabled)",
        value.to_string_lossy()
      );
    }
    if let Some(value) = std::env::var_os("FASTR_WINDOWS_RENDERER_SANDBOX") {
      println!(
        "env: FASTR_WINDOWS_RENDERER_SANDBOX={} (sandbox may be disabled)",
        value.to_string_lossy()
      );
    }

    let exe = match std::env::current_exe() {
      Ok(exe) => exe,
      Err(err) => {
        eprintln!("error: current_exe failed: {err}");
        return 2;
      }
    };

    let mut child_args: Vec<OsString> = Vec::new();
    child_args.push(OsString::from("--child"));

    if let Some(path) = args.read_path.as_ref() {
      child_args.push(OsString::from("--read"));
      child_args.push(path.as_os_str().to_owned());
    }

    let listener = if args.connect_localhost {
      match TcpListener::bind((Ipv4Addr::LOCALHOST, 0)) {
        Ok(listener) => {
          let port = listener.local_addr().map(|addr| addr.port()).unwrap_or(0);
          println!("parent: bound localhost listener at 127.0.0.1:{port}");
          child_args.push(OsString::from("--connect"));
          child_args.push(OsString::from(format!("127.0.0.1:{port}")));
          Some(listener)
        }
        Err(err) => {
          eprintln!("parent: failed to bind localhost listener: {err}");
          None
        }
      }
    } else {
      None
    };

    if let Some(connect) = args.connect.as_ref() {
      child_args.push(OsString::from("--connect"));
      child_args.push(connect.clone());
    }

    let inherit = collect_stdio_handles_for_inheritance();
    println!(
      "parent: spawning child (inherit_handles={}, aap_hardened={}, timeout_ms={})",
      inherit.len(),
      args.aap_hardened,
      args.timeout_ms
    );

    let child = match RendererSandbox::spawn(&exe, &child_args, &inherit, args.aap_hardened) {
      Ok(child) => child,
      Err(err) => {
        eprintln!("error: failed to spawn sandboxed child: {err}");
        return 2;
      }
    };

    println!(
      "parent: spawned child pid={} mode={:?} job_assigned={} breakaway_from_job={}",
      child.pid,
      child.mode,
      child.job_assigned,
      child.used_breakaway_from_job
    );

    // Keep any listener alive until after the child returns from its connect probe.
    let _listener = listener;

    let handle = child.process.as_raw_handle() as windows_sys::Win32::Foundation::HANDLE;
    let exit_code = match wait_process(handle, args.timeout_ms) {
      Ok(code) => code,
      Err(err) => {
        eprintln!("error: failed waiting for child: {err}");
        return 2;
      }
    };
    println!("parent: child exit_code={exit_code}");
    exit_code as i32
  }

  fn run_child(args: &Args) -> i32 {
    println!("== win-sandbox probe (child) ==");
    println!("pid: {}", std::process::id());

    let token = match open_process_token_query() {
      Ok(token) => token,
      Err(err) => {
        eprintln!("token: OpenProcessToken failed: {err}");
        return 2;
      }
    };

    match token_is_appcontainer(token.0) {
      Ok(is_ac) => println!("TokenIsAppContainer: {is_ac}"),
      Err(err) => eprintln!("TokenIsAppContainer: error: {err}"),
    }

    match token_integrity_level(token.0) {
      Ok(il) => println!("IntegrityLevel: {} (rid=0x{:X}, sid={})", il.name, il.rid, il.sid),
      Err(err) => eprintln!("IntegrityLevel: error: {err}"),
    }

    match token_has_group_sid(token.0, "S-1-15-2-1") {
      Ok(has) => println!("TokenHasAllApplicationPackages(S-1-15-2-1): {has}"),
      Err(err) => eprintln!("TokenHasAllApplicationPackages: error: {err}"),
    }

    match token_capability_sids(token.0) {
      Ok(caps) => {
        if caps.is_empty() {
          println!("TokenCapabilities: <empty>");
        } else {
          println!("TokenCapabilities: {caps:?}");
        }
      }
      Err(err) => eprintln!("TokenCapabilities: error: {err}"),
    }

    match current_process_in_job() {
      Ok(in_job) => println!("IsProcessInJob: {in_job}"),
      Err(err) => eprintln!("IsProcessInJob: error: {err}"),
    }

    println!();
    println!("== Process mitigations (GetProcessMitigationPolicy) ==");
    print_mitigations();

    println!();
    println!("== Probes ==");

    if let Some(path) = args.read_path.as_ref() {
      probe_read(path);
    } else {
      println!("fs: read skipped (pass --read <PATH>)");
    }

    if let Some(addr) = args.connect.as_ref() {
      probe_connect(addr);
    } else {
      println!("net: connect skipped (pass --connect <IP:PORT> or --connect-localhost)");
    }

    0
  }

  // -----------------------------------------------------------------------------
  // Parent helpers
  // -----------------------------------------------------------------------------

  fn collect_stdio_handles_for_inheritance() -> Vec<RawHandle> {
    let std_in = unsafe { GetStdHandle(STD_INPUT_HANDLE) };
    let std_out = unsafe { GetStdHandle(STD_OUTPUT_HANDLE) };
    let std_err = unsafe { GetStdHandle(STD_ERROR_HANDLE) };
    let mut inherit = Vec::new();
    for h in [std_in, std_out, std_err] {
      if h.is_null() || h == INVALID_HANDLE_VALUE {
        continue;
      }
      // Ensure the handle is inheritable so the sandbox spawner can forward it via
      // PROC_THREAD_ATTRIBUTE_HANDLE_LIST.
      let _ = unsafe { SetHandleInformation(h, HANDLE_FLAG_INHERIT, HANDLE_FLAG_INHERIT) };
      inherit.push(h as RawHandle);
    }
    inherit
  }
  fn wait_process(handle: windows_sys::Win32::Foundation::HANDLE, timeout_ms: u32) -> io::Result<u32> {
    let wait = unsafe { WaitForSingleObject(handle, timeout_ms) };
    if wait == WAIT_TIMEOUT {
      unsafe {
        let _ = TerminateProcess(handle, 1);
      }
      return Err(io::Error::new(
        io::ErrorKind::TimedOut,
        format!("child timed out after {timeout_ms}ms (terminated)"),
      ));
    }
    if wait != WAIT_OBJECT_0 {
      return Err(io::Error::last_os_error());
    }

    let mut exit_code: u32 = 0;
    let ok = unsafe { GetExitCodeProcess(handle, &mut exit_code) };
    if ok == 0 {
      return Err(io::Error::last_os_error());
    }
    Ok(exit_code)
  }

  // -----------------------------------------------------------------------------
  // Token / job queries
  // -----------------------------------------------------------------------------

  struct HandleGuard(windows_sys::Win32::Foundation::HANDLE);

  impl Drop for HandleGuard {
    fn drop(&mut self) {
      unsafe {
        if !self.0.is_null() {
          CloseHandle(self.0);
        }
      }
    }
  }

  fn open_process_token_query() -> io::Result<HandleGuard> {
    let mut token: windows_sys::Win32::Foundation::HANDLE = std::ptr::null_mut();
    let ok = unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) };
    if ok == 0 {
      return Err(io::Error::last_os_error());
    }
    if token.is_null() {
      return Err(io::Error::new(
        io::ErrorKind::Other,
        "OpenProcessToken returned null handle",
      ));
    }
    Ok(HandleGuard(token))
  }

  fn token_is_appcontainer(token: windows_sys::Win32::Foundation::HANDLE) -> io::Result<bool> {
    let mut is_appcontainer: u32 = 0;
    let mut returned: u32 = 0;
    let ok = unsafe {
      GetTokenInformation(
        token,
        TokenIsAppContainer,
        (&mut is_appcontainer as *mut u32).cast::<c_void>(),
        std::mem::size_of::<u32>() as u32,
        &mut returned,
      )
    };
    if ok == 0 {
      return Err(io::Error::last_os_error());
    }
    Ok(is_appcontainer != 0)
  }

  #[derive(Debug)]
  struct IntegrityLevel {
    name: &'static str,
    rid: u32,
    sid: String,
  }

  fn token_integrity_level(token: windows_sys::Win32::Foundation::HANDLE) -> io::Result<IntegrityLevel> {
    let mut needed: u32 = 0;
    let ok = unsafe {
      GetTokenInformation(
        token,
        TokenIntegrityLevel,
        std::ptr::null_mut(),
        0,
        &mut needed,
      )
    };
    if ok != 0 {
      return Err(io::Error::new(
        io::ErrorKind::Other,
        "GetTokenInformation(TokenIntegrityLevel) unexpectedly succeeded with null buffer",
      ));
    }
    if needed == 0 {
      return Err(io::Error::last_os_error());
    }

    // Ensure pointer alignment.
    let word_count = (needed as usize + std::mem::size_of::<usize>() - 1) / std::mem::size_of::<usize>();
    let mut buffer_words = vec![0usize; word_count];
    let buffer_ptr = buffer_words.as_mut_ptr().cast::<c_void>();

    let ok = unsafe {
      GetTokenInformation(
        token,
        TokenIntegrityLevel,
        buffer_ptr,
        needed,
        &mut needed,
      )
    };
    if ok == 0 {
      return Err(io::Error::last_os_error());
    }

    let tml = buffer_ptr.cast::<TOKEN_MANDATORY_LABEL>();
    let sid = unsafe { (*tml).Label.Sid as PSID };
    if sid.is_null() {
      return Err(io::Error::new(
        io::ErrorKind::Other,
        "TokenIntegrityLevel returned null SID",
      ));
    }

    let rid = unsafe { integrity_rid_from_sid(sid)? };
    let name = integrity_level_name(rid);
    let sid_str = unsafe { sid_to_string(sid)? };

    Ok(IntegrityLevel {
      name,
      rid,
      sid: sid_str,
    })
  }

  unsafe fn integrity_rid_from_sid(sid: PSID) -> io::Result<u32> {
    let count_ptr = GetSidSubAuthorityCount(sid);
    if count_ptr.is_null() {
      return Err(io::Error::last_os_error());
    }
    let count = *count_ptr as u32;
    if count == 0 {
      return Err(io::Error::new(
        io::ErrorKind::Other,
        "SID has no subauthorities",
      ));
    }
    let rid_ptr = GetSidSubAuthority(sid, count - 1);
    if rid_ptr.is_null() {
      return Err(io::Error::last_os_error());
    }
    Ok(*rid_ptr)
  }

  fn integrity_level_name(rid: u32) -> &'static str {
    // Windows integrity levels are encoded in the RID of the mandatory label SID (S-1-16-...).
    match rid {
      0x0000 => "Untrusted",
      0x1000 => "Low",
      0x2000 => "Medium",
      0x3000 => "High",
      0x4000 => "System",
      0x5000 => "ProtectedProcess",
      _ => {
        if rid < 0x1000 {
          "Untrusted(?)"
        } else if rid < 0x2000 {
          "Low(?)"
        } else if rid < 0x3000 {
          "Medium(?)"
        } else if rid < 0x4000 {
          "High(?)"
        } else if rid < 0x5000 {
          "System(?)"
        } else {
          "Unknown"
        }
      }
    }
  }

  unsafe fn sid_to_string(sid: PSID) -> io::Result<String> {
    let mut sid_str: *mut u16 = std::ptr::null_mut();
    let ok = ConvertSidToStringSidW(sid, &mut sid_str);
    if ok == 0 || sid_str.is_null() {
      return Err(io::Error::last_os_error());
    }
    let mut len = 0usize;
    while *sid_str.add(len) != 0 {
      len += 1;
    }
    let wide = std::slice::from_raw_parts(sid_str, len);
    let out = String::from_utf16_lossy(wide);
    windows_sys::Win32::Foundation::LocalFree(sid_str as _);
    Ok(out)
  }

  fn token_has_group_sid(
    token: windows_sys::Win32::Foundation::HANDLE,
    want_sid: &str,
  ) -> io::Result<bool> {
    let mut needed: u32 = 0;
    let ok = unsafe {
      GetTokenInformation(token, TokenGroups, std::ptr::null_mut(), 0, &mut needed)
    };
    if ok != 0 {
      return Err(io::Error::new(
        io::ErrorKind::Other,
        "GetTokenInformation(TokenGroups) unexpectedly succeeded with null buffer",
      ));
    }
    if needed == 0 {
      return Err(io::Error::last_os_error());
    }

    // Ensure pointer alignment.
    let word_count =
      (needed as usize + std::mem::size_of::<usize>() - 1) / std::mem::size_of::<usize>();
    let mut buffer_words = vec![0usize; word_count.max(1)];
    let buffer_ptr = buffer_words.as_mut_ptr().cast::<c_void>();

    let ok = unsafe { GetTokenInformation(token, TokenGroups, buffer_ptr, needed, &mut needed) };
    if ok == 0 {
      return Err(io::Error::last_os_error());
    }

    let groups = buffer_ptr.cast::<TOKEN_GROUPS>();
    let count = unsafe { (*groups).GroupCount as usize };
    let first = unsafe { (*groups).Groups.as_ptr() as *const SID_AND_ATTRIBUTES };
    let entries = unsafe { std::slice::from_raw_parts(first, count) };
    for entry in entries {
      let sid = entry.Sid as PSID;
      if sid.is_null() {
        continue;
      }
      let sid_str = unsafe { sid_to_string(sid)? };
      if sid_str == want_sid {
        return Ok(true);
      }
    }
    Ok(false)
  }

  fn token_capability_sids(
    token: windows_sys::Win32::Foundation::HANDLE,
  ) -> io::Result<Vec<String>> {
    let mut needed: u32 = 0;
    let ok = unsafe {
      GetTokenInformation(
        token,
        TokenCapabilities,
        std::ptr::null_mut(),
        0,
        &mut needed,
      )
    };
    if ok != 0 {
      return Err(io::Error::new(
        io::ErrorKind::Other,
        "GetTokenInformation(TokenCapabilities) unexpectedly succeeded with null buffer",
      ));
    }
    if needed == 0 {
      return Err(io::Error::last_os_error());
    }

    // Ensure pointer alignment.
    let word_count =
      (needed as usize + std::mem::size_of::<usize>() - 1) / std::mem::size_of::<usize>();
    let mut buffer_words = vec![0usize; word_count.max(1)];
    let buffer_ptr = buffer_words.as_mut_ptr().cast::<c_void>();

    let ok = unsafe {
      GetTokenInformation(
        token,
        TokenCapabilities,
        buffer_ptr,
        needed,
        &mut needed,
      )
    };
    if ok == 0 {
      return Err(io::Error::last_os_error());
    }

    let groups = buffer_ptr.cast::<TOKEN_GROUPS>();
    let count = unsafe { (*groups).GroupCount as usize };
    let first = unsafe { (*groups).Groups.as_ptr() as *const SID_AND_ATTRIBUTES };
    let entries = unsafe { std::slice::from_raw_parts(first, count) };

    let mut out = Vec::new();
    for entry in entries {
      let sid = entry.Sid as PSID;
      if sid.is_null() {
        continue;
      }
      let sid_str = unsafe { sid_to_string(sid)? };
      out.push(sid_str);
    }
    Ok(out)
  }

  fn current_process_in_job() -> io::Result<bool> {
    let mut in_job: i32 = FALSE;
    let ok = unsafe { IsProcessInJob(GetCurrentProcess(), std::ptr::null_mut(), &mut in_job) };
    if ok == 0 {
      return Err(io::Error::last_os_error());
    }
    Ok(in_job != 0)
  }

  // -----------------------------------------------------------------------------
  // Mitigation printing
  // -----------------------------------------------------------------------------

  #[allow(non_snake_case)]
  #[repr(C)]
  struct PROCESS_MITIGATION_DYNAMIC_CODE_POLICY {
    Flags: u32,
  }

  #[allow(non_snake_case)]
  #[repr(C)]
  struct PROCESS_MITIGATION_EXTENSION_POINT_DISABLE_POLICY {
    Flags: u32,
  }

  #[allow(non_snake_case)]
  #[repr(C)]
  struct PROCESS_MITIGATION_SYSTEM_CALL_DISABLE_POLICY {
    Flags: u32,
  }

  #[allow(non_snake_case)]
  #[repr(C)]
  struct PROCESS_MITIGATION_IMAGE_LOAD_POLICY {
    Flags: u32,
  }

  #[allow(non_snake_case)]
  #[repr(C)]
  struct PROCESS_MITIGATION_STRICT_HANDLE_CHECK_POLICY {
    Flags: u32,
  }

  type GetProcessMitigationPolicyFn = unsafe extern "system" fn(
    windows_sys::Win32::Foundation::HANDLE,
    PROCESS_MITIGATION_POLICY,
    *mut c_void,
    usize,
  ) -> i32;

  fn get_process_mitigation_policy_fn() -> Option<GetProcessMitigationPolicyFn> {
    use std::sync::OnceLock;
    use windows_sys::Win32::Foundation::HMODULE;
    use windows_sys::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress};

    // `GetProcessMitigationPolicy` is only exported on Windows 8+. Load it dynamically so this probe
    // can still run on downlevel Windows builds (where mitigations are treated as unsupported).
    static FN: OnceLock<Option<GetProcessMitigationPolicyFn>> = OnceLock::new();
    *FN.get_or_init(|| unsafe {
      let kernel32: Vec<u16> = "kernel32.dll\0".encode_utf16().collect();
      let module: HMODULE = GetModuleHandleW(kernel32.as_ptr());
      if module.is_null() {
        return None;
      }

      let proc = GetProcAddress(module, b"GetProcessMitigationPolicy\0".as_ptr())?;
      Some(std::mem::transmute::<_, GetProcessMitigationPolicyFn>(proc))
    })
  }

  fn get_mitigation_policy<T>(policy: PROCESS_MITIGATION_POLICY) -> io::Result<T> {
    let Some(get_policy) = get_process_mitigation_policy_fn() else {
      return Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "GetProcessMitigationPolicy unavailable (requires Windows 8+)",
      ));
    };

    let mut data: T = unsafe { std::mem::zeroed() };
    let ok = unsafe {
      get_policy(
        GetCurrentProcess(),
        policy,
        (&mut data as *mut T).cast::<c_void>(),
        std::mem::size_of::<T>(),
      )
    };
    if ok == 0 {
      return Err(io::Error::last_os_error());
    }
    Ok(data)
  }

  fn print_mitigations() {
    // Report the mitigations we care about for the renderer sandbox.
    print_policy::<_, PROCESS_MITIGATION_DYNAMIC_CODE_POLICY>(
      "ProcessDynamicCodePolicy",
      ProcessDynamicCodePolicy,
      |flags| {
        let prohibit = flags & 0x1 != 0;
        format!("prohibit_dynamic_code={prohibit}")
      },
    );
    print_policy::<_, PROCESS_MITIGATION_EXTENSION_POINT_DISABLE_POLICY>(
      "ProcessExtensionPointDisablePolicy",
      ProcessExtensionPointDisablePolicy,
      |flags| {
        let disabled = flags & 0x1 != 0;
        format!("disable_extension_points={disabled}")
      },
    );
    print_policy::<_, PROCESS_MITIGATION_SYSTEM_CALL_DISABLE_POLICY>(
      "ProcessSystemCallDisablePolicy",
      ProcessSystemCallDisablePolicy,
      |flags| {
        let win32k = flags & 0x1 != 0;
        format!("disallow_win32k_system_calls={win32k}")
      },
    );
    print_policy::<_, PROCESS_MITIGATION_IMAGE_LOAD_POLICY>(
      "ProcessImageLoadPolicy",
      ProcessImageLoadPolicy,
      |flags| {
        let no_remote = flags & 0x1 != 0;
        let no_low = flags & 0x2 != 0;
        let prefer_system32 = flags & 0x4 != 0;
        format!(
          "no_remote_images={no_remote} no_low_mandatory_label_images={no_low} prefer_system32_images={prefer_system32}"
        )
      },
    );
    print_policy::<_, PROCESS_MITIGATION_STRICT_HANDLE_CHECK_POLICY>(
      "ProcessStrictHandleCheckPolicy",
      ProcessStrictHandleCheckPolicy,
      |flags| {
        let raise = flags & 0x1 != 0;
        format!("raise_exception_on_invalid_handle_reference={raise}")
      },
    );
  }

  fn print_policy<F, T>(name: &str, policy: PROCESS_MITIGATION_POLICY, describe: F)
  where
    F: FnOnce(u32) -> String,
    T: PolicyFlags,
  {
    match get_mitigation_policy::<T>(policy) {
      Ok(p) => println!("{name}: flags=0x{:08X} {}", p.flags(), describe(p.flags())),
      Err(err) => println!("{name}: unavailable ({err})"),
    }
  }

  trait PolicyFlags {
    fn flags(&self) -> u32;
  }

  impl PolicyFlags for PROCESS_MITIGATION_DYNAMIC_CODE_POLICY {
    fn flags(&self) -> u32 {
      self.Flags
    }
  }
  impl PolicyFlags for PROCESS_MITIGATION_EXTENSION_POINT_DISABLE_POLICY {
    fn flags(&self) -> u32 {
      self.Flags
    }
  }
  impl PolicyFlags for PROCESS_MITIGATION_SYSTEM_CALL_DISABLE_POLICY {
    fn flags(&self) -> u32 {
      self.Flags
    }
  }
  impl PolicyFlags for PROCESS_MITIGATION_IMAGE_LOAD_POLICY {
    fn flags(&self) -> u32 {
      self.Flags
    }
  }
  impl PolicyFlags for PROCESS_MITIGATION_STRICT_HANDLE_CHECK_POLICY {
    fn flags(&self) -> u32 {
      self.Flags
    }
  }

  // -----------------------------------------------------------------------------
  // Probes
  // -----------------------------------------------------------------------------

  fn describe_win32_error(code: i32) -> Option<&'static str> {
    match code as u32 {
      ERROR_ACCESS_DENIED => Some("ERROR_ACCESS_DENIED"),
      2 => Some("ERROR_FILE_NOT_FOUND"),
      3 => Some("ERROR_PATH_NOT_FOUND"),
      32 => Some("ERROR_SHARING_VIOLATION"),
      _ => None,
    }
  }

  fn describe_wsa_error(code: i32) -> Option<&'static str> {
    match code {
      10013 => Some("WSAEACCES"),
      10051 => Some("WSAENETUNREACH"),
      10060 => Some("WSAETIMEDOUT"),
      10061 => Some("WSAECONNREFUSED"),
      10065 => Some("WSAEHOSTUNREACH"),
      _ => None,
    }
  }

  fn format_os_error(raw: Option<i32>, describe: fn(i32) -> Option<&'static str>) -> String {
    match raw {
      Some(code) => match describe(code) {
        Some(name) => format!("Some({code}) ({name})"),
        None => format!("Some({code})"),
      },
      None => "None".to_string(),
    }
  }

  fn probe_read(path: &PathBuf) {
    match std::fs::read(path) {
      Ok(bytes) => println!(
        "fs: read {} bytes from {} (SUCCESS)",
        bytes.len(),
        path.display()
      ),
      Err(err) => {
        let raw = err.raw_os_error();
        println!(
          "fs: read {} (FAILED): {} (raw_os_error={})",
          path.display(),
          err,
          format_os_error(raw, describe_win32_error)
        );
      }
    }
  }

  fn probe_connect(raw: &OsString) {
    let raw_str = raw.to_string_lossy();
    let addr: SocketAddr = match raw_str.parse() {
      Ok(addr) => addr,
      Err(_) => {
        println!("net: connect {raw_str} (SKIPPED): expected IP:PORT");
        return;
      }
    };
    let timeout = Duration::from_millis(500);
    match TcpStream::connect_timeout(&addr, timeout) {
      Ok(_stream) => println!("net: connect {addr} (SUCCESS)"),
      Err(err) => {
        let raw = err.raw_os_error();
        println!(
          "net: connect {addr} (FAILED): {} (raw_os_error={})",
          err,
          format_os_error(raw, describe_wsa_error)
        );
      }
    }
  }
}
