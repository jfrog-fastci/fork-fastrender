#[cfg(not(windows))]
fn main() {}

#[cfg(windows)]
fn main() {
  if std::env::args_os().any(|a| a == "--grandchild") {
    // Grandchild payload: do nothing.
    std::process::exit(0);
  }

  match run_probe() {
    Ok(()) => std::process::exit(0),
    Err(code) => std::process::exit(code),
  }
}

#[cfg(windows)]
fn run_probe() -> Result<(), i32> {
  use windows_sys::Win32::Foundation::{
    CloseHandle, GetLastError, ERROR_INSUFFICIENT_BUFFER,
  };
  use windows_sys::Win32::Security::{
    GetTokenInformation, SID_AND_ATTRIBUTES, TokenGroups, TokenIsAppContainer, TOKEN_GROUPS,
    TOKEN_INFORMATION_CLASS, TOKEN_QUERY,
  };
  use windows_sys::Win32::System::JobObjects::IsProcessInJob;
  use windows_sys::Win32::System::Threading::{
    DeleteProcThreadAttributeList, GetCurrentProcess, InitializeProcThreadAttributeList,
    UpdateProcThreadAttribute, LPPROC_THREAD_ATTRIBUTE_LIST,
  };
  use win_sandbox::{mitigations, sid_to_string};

  // ProcThreadAttributeValue(15, FALSE, TRUE, FALSE) → 0x0002_000F.
  const PROC_THREAD_ATTRIBUTE_ALL_APPLICATION_PACKAGES_POLICY: usize = 0x0002_000F;
  const PROCESS_CREATION_ALL_APPLICATION_PACKAGES_POLICY_BLOCK: u32 = 1;
  const ALL_APPLICATION_PACKAGES_SID: &str = "S-1-15-2-1";

  #[link(name = "advapi32")]
  extern "system" {
    fn OpenProcessToken(
      process_handle: windows_sys::Win32::Foundation::HANDLE,
      desired_access: u32,
      token_handle: *mut windows_sys::Win32::Foundation::HANDLE,
    ) -> i32;
  }

  fn is_all_application_packages_policy_supported() -> Result<bool, String> {
    // Query required size.
    let mut size: usize = 0;
    unsafe {
      InitializeProcThreadAttributeList(core::ptr::null_mut(), 1, 0, &mut size);
    }
    if size == 0 {
      return Ok(false);
    }

    // Allocate a suitably aligned backing buffer.
    let word_count = (size + core::mem::size_of::<u64>() - 1) / core::mem::size_of::<u64>();
    let mut buffer = vec![0u64; word_count.max(1)];
    let list: LPPROC_THREAD_ATTRIBUTE_LIST = buffer.as_mut_ptr().cast();

    let ok = unsafe { InitializeProcThreadAttributeList(list, 1, 0, &mut size) };
    if ok == 0 {
      return Err(format!(
        "InitializeProcThreadAttributeList failed: {}",
        std::io::Error::last_os_error()
      ));
    }

    let mut policy_value = PROCESS_CREATION_ALL_APPLICATION_PACKAGES_POLICY_BLOCK;
    let ok = unsafe {
      UpdateProcThreadAttribute(
        list,
        0,
        PROC_THREAD_ATTRIBUTE_ALL_APPLICATION_PACKAGES_POLICY,
        core::ptr::addr_of_mut!(policy_value).cast(),
        core::mem::size_of::<u32>(),
        core::ptr::null_mut(),
        core::ptr::null_mut(),
      )
    };

    let supported = if ok != 0 {
      true
    } else {
      let code = unsafe { GetLastError() };
      // ERROR_NOT_SUPPORTED (50) / ERROR_INVALID_PARAMETER (87) => attribute not available.
      if code == 50 || code == 87 {
        false
      } else {
        unsafe {
          DeleteProcThreadAttributeList(list);
        }
        return Err(format!(
          "UpdateProcThreadAttribute(PROC_THREAD_ATTRIBUTE_ALL_APPLICATION_PACKAGES_POLICY) failed with Win32 error {code}"
        ));
      }
    };

    unsafe {
      DeleteProcThreadAttributeList(list);
    }
    Ok(supported)
  }

  fn get_token_information(
    token: windows_sys::Win32::Foundation::HANDLE,
    class: TOKEN_INFORMATION_CLASS,
  ) -> Result<Vec<u8>, String> {
    let mut needed: u32 = 0;
    let ok = unsafe {
      GetTokenInformation(
        token,
        class,
        core::ptr::null_mut(),
        0,
        core::ptr::addr_of_mut!(needed),
      )
    };
    if ok != 0 {
      // Unexpected but possible for fixed-size info classes.
      return Ok(Vec::new());
    }

    let err = std::io::Error::last_os_error();
    if err.raw_os_error() != Some(ERROR_INSUFFICIENT_BUFFER as i32) {
      return Err(format!(
        "GetTokenInformation(size query) failed: {err} (raw_os_error={:?})",
        err.raw_os_error()
      ));
    }
    if needed == 0 {
      return Err(
        "GetTokenInformation returned ERROR_INSUFFICIENT_BUFFER but length was 0".to_string(),
      );
    }

    let mut buf = vec![0u8; needed as usize];
    let ok = unsafe {
      GetTokenInformation(
        token,
        class,
        buf.as_mut_ptr().cast(),
        needed,
        core::ptr::addr_of_mut!(needed),
      )
    };
    if ok == 0 {
      return Err(format!(
        "GetTokenInformation(data) failed: {}",
        std::io::Error::last_os_error()
      ));
    }
    buf.truncate(needed as usize);
    Ok(buf)
  }

  fn token_group_sids(token: windows_sys::Win32::Foundation::HANDLE) -> Result<Vec<String>, String> {
    let buf = get_token_information(token, TokenGroups as TOKEN_INFORMATION_CLASS)?;
    if buf.is_empty() {
      return Ok(Vec::new());
    }
    if buf.len() < core::mem::size_of::<TOKEN_GROUPS>() {
      return Err(format!(
        "TokenGroups buffer too small ({} bytes)",
        buf.len()
      ));
    }

    // SAFETY: buffer is large enough for TOKEN_GROUPS header.
    let groups = unsafe { &*(buf.as_ptr().cast::<TOKEN_GROUPS>()) };
    let count = groups.GroupCount as usize;
    let first = groups.Groups.as_ptr().cast::<SID_AND_ATTRIBUTES>();

    let slice = unsafe { core::slice::from_raw_parts(first, count) };
    let mut out = Vec::with_capacity(count);
    for entry in slice {
      if entry.Sid.is_null() {
        continue;
      }
      out.push(sid_to_string(entry.Sid).map_err(|err| err.to_string())?);
    }
    Ok(out)
  }

  // 1) AppContainer check.
  let mut token: windows_sys::Win32::Foundation::HANDLE = core::ptr::null_mut();
  let ok = unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) };
  if ok == 0 {
    eprintln!(
      "OpenProcessToken failed: {}",
      std::io::Error::last_os_error()
    );
    return Err(10);
  }

  let mut is_appcontainer: u32 = 0;
  let mut ret_len: u32 = 0;
  let ok = unsafe {
    GetTokenInformation(
      token,
      TokenIsAppContainer,
      &mut is_appcontainer as *mut _ as *mut core::ffi::c_void,
      core::mem::size_of::<u32>() as u32,
      &mut ret_len,
    )
  };
  if ok == 0 {
    eprintln!(
      "GetTokenInformation(TokenIsAppContainer) failed: {}",
      std::io::Error::last_os_error()
    );
    unsafe { CloseHandle(token) };
    return Err(11);
  }
  if is_appcontainer == 0 {
    eprintln!("not running inside an AppContainer");
    unsafe { CloseHandle(token) };
    return Err(12);
  }

  // 2) ALL APPLICATION PACKAGES hardening check (best-effort).
  //
  // The renderer sandbox spawners attempt to remove `ALL APPLICATION PACKAGES` (S-1-15-2-1) from the
  // created AppContainer token via `PROC_THREAD_ATTRIBUTE_ALL_APPLICATION_PACKAGES_POLICY`. If the
  // host rejects this attribute, treat it as unsupported rather than failing the sandbox.
  let groups = match token_group_sids(token) {
    Ok(groups) => groups,
    Err(err) => {
      eprintln!("query TokenGroups failed: {err}");
      unsafe { CloseHandle(token) };
      return Err(23);
    }
  };

  let has_aap = groups
    .iter()
    .any(|sid| sid.eq_ignore_ascii_case(ALL_APPLICATION_PACKAGES_SID));
  if has_aap {
    match is_all_application_packages_policy_supported() {
      Ok(false) => {
        eprintln!(
          "note: host does not support PROC_THREAD_ATTRIBUTE_ALL_APPLICATION_PACKAGES_POLICY; token contains {ALL_APPLICATION_PACKAGES_SID}"
        );
      }
      Ok(true) => {
        eprintln!(
          "expected AppContainer token to omit ALL APPLICATION PACKAGES ({ALL_APPLICATION_PACKAGES_SID}); groups={groups:?}"
        );
        unsafe { CloseHandle(token) };
        return Err(23);
      }
      Err(err) => {
        eprintln!("failed to probe AAP hardening attribute support: {err}");
        unsafe { CloseHandle(token) };
        return Err(23);
      }
    }
  }

  unsafe { CloseHandle(token) };

  // 3) Job membership check.
  let mut in_job: i32 = 0;
  let ok = unsafe { IsProcessInJob(GetCurrentProcess(), core::ptr::null_mut(), &mut in_job) };
  if ok == 0 {
    eprintln!("IsProcessInJob failed: {}", std::io::Error::last_os_error());
    return Err(20);
  }
  if in_job == 0 {
    eprintln!("not running inside a Job object");
    return Err(21);
  }

  // 4) Mitigations check (best-effort; expected mask is computed based on OS support).
  if let Err(err) = mitigations::verify_renderer_mitigations_current_process() {
    eprintln!("renderer mitigations not active: {err}");
    return Err(22);
  }

  // 5) Grandchild spawn should fail (active process limit 1, no breakaway).
  let exe = std::env::current_exe().map_err(|_| 30)?;
  let mut cmd = std::process::Command::new(exe);
  cmd.arg("--grandchild");

  match cmd.spawn() {
    Ok(mut child) => {
      let _ = child.kill();
      let _ = child.wait();
      eprintln!("grandchild spawn unexpectedly succeeded");
      Err(31)
    }
    Err(err) => {
      // Typical failure is ERROR_ACCESS_DENIED (5).
      let _ = err;
      Ok(())
    }
  }
}
