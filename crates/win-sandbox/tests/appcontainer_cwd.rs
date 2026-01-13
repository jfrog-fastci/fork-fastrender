#![cfg(windows)]

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::time::Duration;

use tempfile::tempdir;

use win_sandbox::{spawn_sandboxed, AppContainerProfile, SpawnConfig};

use windows_sys::Win32::Foundation::{
  CloseHandle, LocalFree, ERROR_ACCESS_DENIED, ERROR_FILE_NOT_FOUND, ERROR_PATH_NOT_FOUND, HANDLE,
};
use windows_sys::Win32::Security::Authorization::{
  ConvertStringSidToSidW, SetEntriesInAclW, SetNamedSecurityInfoW, EXPLICIT_ACCESS_W, GRANT_ACCESS,
  NO_MULTIPLE_TRUSTEE, SE_FILE_OBJECT, TRUSTEE_IS_SID, TRUSTEE_IS_UNKNOWN, TRUSTEE_W,
};
use windows_sys::Win32::Security::{
  GetTokenInformation, TokenIsAppContainer, DACL_SECURITY_INFORMATION, PSID, TOKEN_QUERY,
};
use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

mod common;

const TEST_NAME: &str = "appcontainer_spawn_does_not_inherit_inaccessible_cwd";
const ENV_CHILD: &str = "WIN_SANDBOX_APPCONTAINER_CWD_CHILD";
const ENV_CWD: &str = "WIN_SANDBOX_APPCONTAINER_CWD_PATH";

fn wide_from_os(value: &OsStr) -> Vec<u16> {
  use std::os::windows::ffi::OsStrExt;
  value.encode_wide().chain(std::iter::once(0)).collect()
}

fn wide_from_str(value: &str) -> Vec<u16> {
  value.encode_utf16().chain(std::iter::once(0)).collect()
}

fn set_users_only_dacl(path: &Path) -> std::io::Result<()> {
  // Grant access to the "Users" group only (S-1-5-32-545). AppContainer tokens do not have this
  // group enabled, so access checks should fail.
  const USERS_SID: &str = "S-1-5-32-545";
  // Generic access rights from winnt.h.
  const GENERIC_ALL: u32 = 0x1000_0000;
  // `NO_INHERITANCE` from accctrl.h.
  const NO_INHERITANCE: u32 = 0;
  // `PROTECTED_DACL_SECURITY_INFORMATION` from winnt.h (disable DACL inheritance).
  const PROTECTED_DACL_SECURITY_INFORMATION: u32 = 0x8000_0000;

  let mut users_sid: PSID = std::ptr::null_mut();
  let sid_w = wide_from_str(USERS_SID);
  let ok = unsafe { ConvertStringSidToSidW(sid_w.as_ptr(), &mut users_sid) };
  if ok == 0 {
    return Err(std::io::Error::last_os_error());
  }
  if users_sid.is_null() {
    return Err(std::io::Error::new(
      std::io::ErrorKind::Other,
      "ConvertStringSidToSidW returned null SID",
    ));
  }

  struct SidGuard(PSID);
  impl Drop for SidGuard {
    fn drop(&mut self) {
      unsafe {
        if !self.0.is_null() {
          LocalFree(self.0 as _);
        }
      }
    }
  }
  let _sid_guard = SidGuard(users_sid);

  let mut ea: EXPLICIT_ACCESS_W = unsafe { std::mem::zeroed() };
  ea.grfAccessPermissions = GENERIC_ALL;
  ea.grfAccessMode = GRANT_ACCESS;
  ea.grfInheritance = NO_INHERITANCE;
  ea.Trustee = TRUSTEE_W {
    pMultipleTrustee: std::ptr::null_mut(),
    MultipleTrusteeOperation: NO_MULTIPLE_TRUSTEE,
    TrusteeForm: TRUSTEE_IS_SID,
    TrusteeType: TRUSTEE_IS_UNKNOWN,
    ptstrName: users_sid as *mut _,
  };

  let mut new_dacl: *mut windows_sys::Win32::Security::ACL = std::ptr::null_mut();
  let status = unsafe { SetEntriesInAclW(1, &mut ea, std::ptr::null_mut(), &mut new_dacl) };
  if status != 0 {
    return Err(std::io::Error::from_raw_os_error(status as i32));
  }
  if new_dacl.is_null() {
    return Err(std::io::Error::new(
      std::io::ErrorKind::Other,
      "SetEntriesInAclW returned null ACL",
    ));
  }

  struct AclGuard(*mut windows_sys::Win32::Security::ACL);
  impl Drop for AclGuard {
    fn drop(&mut self) {
      unsafe {
        if !self.0.is_null() {
          LocalFree(self.0 as _);
        }
      }
    }
  }
  let _acl_guard = AclGuard(new_dacl);

  let mut name = wide_from_os(path.as_os_str());
  let status = unsafe {
    SetNamedSecurityInfoW(
      name.as_mut_ptr(),
      SE_FILE_OBJECT,
      DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
      std::ptr::null_mut(),
      std::ptr::null_mut(),
      new_dacl,
      std::ptr::null_mut(),
    )
  };
  if status != 0 {
    return Err(std::io::Error::from_raw_os_error(status as i32));
  }

  Ok(())
}

fn current_process_is_appcontainer() -> bool {
  unsafe {
    let mut token: HANDLE = std::ptr::null_mut();
    let ok = OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token);
    if ok == 0 || token.is_null() {
      return false;
    }

    struct TokenGuard(HANDLE);
    impl Drop for TokenGuard {
      fn drop(&mut self) {
        unsafe {
          let _ = CloseHandle(self.0);
        }
      }
    }
    let _token_guard = TokenGuard(token);

    let mut is_appcontainer: u32 = 0;
    let mut returned: u32 = 0;
    let ok = GetTokenInformation(
      token,
      TokenIsAppContainer,
      std::ptr::addr_of_mut!(is_appcontainer).cast(),
      std::mem::size_of::<u32>() as u32,
      std::ptr::addr_of_mut!(returned),
    );
    ok != 0 && is_appcontainer != 0
  }
}

fn normalize_path_lossy(path: &Path) -> String {
  path
    .to_string_lossy()
    .replace('/', "\\")
    .trim_end_matches('\\')
    .to_ascii_lowercase()
}

#[test]
fn appcontainer_spawn_does_not_inherit_inaccessible_cwd() {
  if std::env::var_os(ENV_CHILD).is_some() {
    assert!(
      current_process_is_appcontainer(),
      "expected child process to run under an AppContainer token (no silent sandbox downgrade)"
    );

    let blocked = std::env::var_os(ENV_CWD).expect("missing blocked CWD env var in child");
    let blocked_path = PathBuf::from(blocked);

    let current = std::env::current_dir().expect("current_dir in child");
    assert_ne!(
      normalize_path_lossy(&current),
      normalize_path_lossy(&blocked_path),
      "AppContainer child inherited the parent's (restricted) current directory"
    );

    let err = std::env::set_current_dir(&blocked_path)
      .expect_err("expected set_current_dir to blocked parent directory to fail in AppContainer");
    if let Some(code) = err.raw_os_error() {
      assert!(
        code == ERROR_ACCESS_DENIED as i32
          || code == ERROR_PATH_NOT_FOUND as i32
          || code == ERROR_FILE_NOT_FOUND as i32,
        "unexpected error when setting blocked CWD: {err:?}"
      );
    }

    return;
  }

  if !common::require_appcontainer_profile(TEST_NAME) {
    return;
  }

  let tmp = tempdir().expect("tempdir");
  set_users_only_dacl(tmp.path()).expect("set Users-only DACL on temp dir");

  let prev_dir = std::env::current_dir().expect("current_dir");
  struct CwdGuard(PathBuf);
  impl Drop for CwdGuard {
    fn drop(&mut self) {
      let _ = std::env::set_current_dir(&self.0);
    }
  }
  std::env::set_current_dir(tmp.path()).expect("set_current_dir to restricted dir");
  let _cwd_guard = CwdGuard(prev_dir);

  let profile = AppContainerProfile::ensure(
    "FastRender.Renderer",
    "FastRender Renderer",
    "FastRender renderer AppContainer profile",
  )
  .expect("ensure AppContainer profile");
  assert!(profile.is_enabled(), "expected enabled AppContainer profile");

  let exe = std::env::current_exe().expect("current test exe path");
  let cfg = SpawnConfig {
    exe,
    args: vec![
      OsString::from("--exact"),
      OsString::from(TEST_NAME),
      OsString::from("--nocapture"),
    ],
    env: vec![
      (OsString::from(ENV_CHILD), OsString::from("1")),
      (OsString::from(ENV_CWD), tmp.path().as_os_str().to_os_string()),
      // Keep subprocess deterministic.
      (OsString::from("RUST_TEST_THREADS"), OsString::from("1")),
    ],
    current_dir: None,
    inherit_handles: Vec::new(),
    appcontainer: Some(profile),
    job: None,
    mitigation_policy: None,
    all_application_packages_hardened: true,
  };

  let child = spawn_sandboxed(&cfg).expect("spawn AppContainer child");
  let exit_code = child
    .wait_timeout(Duration::from_secs(30))
    .expect("wait for child")
    .unwrap_or_else(|| {
      let _ = child.kill();
      panic!("timed out waiting for AppContainer child to exit");
    });
  assert_eq!(exit_code, 0, "child should exit successfully");
}
