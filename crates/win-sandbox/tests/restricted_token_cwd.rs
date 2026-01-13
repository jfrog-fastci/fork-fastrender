#![cfg(windows)]

use std::ffi::{OsStr, OsString};
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use tempfile::tempdir;

use win_sandbox::{restricted_token, RestrictedToken, SpawnConfig};

use windows_sys::Win32::Foundation::{
  CloseHandle, LocalFree, ERROR_ACCESS_DENIED, ERROR_FILE_NOT_FOUND, ERROR_PATH_NOT_FOUND, HANDLE,
};
use windows_sys::Win32::Security::Authorization::{
  ConvertStringSidToSidW, SetEntriesInAclW, SetNamedSecurityInfoW, EXPLICIT_ACCESS_W, GRANT_ACCESS,
  NO_MULTIPLE_TRUSTEE, SE_FILE_OBJECT, TRUSTEE_IS_SID, TRUSTEE_IS_UNKNOWN, TRUSTEE_W,
};
use windows_sys::Win32::Security::{
  GetSidSubAuthority, GetSidSubAuthorityCount, GetTokenInformation, TokenIntegrityLevel,
  PSID, TOKEN_MANDATORY_LABEL, TOKEN_QUERY,
};
use windows_sys::Win32::System::Threading::GetCurrentProcess;

const TEST_NAME: &str = "restricted_token_spawn_does_not_inherit_inaccessible_cwd";
const ENV_TEST_DEPTH: &str = "WIN_SANDBOX_RESTRICTED_TOKEN_CWD_DEPTH";
const ENV_TEST_CWD: &str = "WIN_SANDBOX_RESTRICTED_TOKEN_CWD_PATH";

#[link(name = "advapi32")]
extern "system" {
  fn OpenProcessToken(process: HANDLE, desired_access: u32, token: *mut HANDLE) -> i32;
}

fn wide_from_os(value: &OsStr) -> Vec<u16> {
  value.encode_wide().chain(std::iter::once(0)).collect()
}

fn wide_from_str(value: &str) -> Vec<u16> {
  value.encode_utf16().chain(std::iter::once(0)).collect()
}

fn set_users_only_dacl(path: &Path) -> std::io::Result<()> {
  // Grant access to the "Users" group only (S-1-5-32-545). The restricted token disables this
  // group, so access checks should fail.
  const USERS_SID: &str = "S-1-5-32-545";
  // Generic access rights from winnt.h.
  const GENERIC_ALL: u32 = 0x1000_0000;
  // `NO_INHERITANCE` from `accctrl.h` is defined as 0. Keep a local copy so this test does not rely
  // on `windows-sys` exporting the constant.
  const NO_INHERITANCE: u32 = 0;
  // `DACL_SECURITY_INFORMATION` from winnt.h.
  const DACL_SECURITY_INFORMATION: u32 = 0x0000_0004;
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

fn current_integrity_rid() -> u32 {
  let mut token: HANDLE = std::ptr::null_mut();
  let ok = unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) };
  assert_ne!(
    ok,
    0,
    "OpenProcessToken failed: {}",
    std::io::Error::last_os_error()
  );
  assert!(
    !token.is_null(),
    "OpenProcessToken returned null token handle"
  );

  let mut len: u32 = 0;
  unsafe {
    GetTokenInformation(
      token,
      TokenIntegrityLevel,
      std::ptr::null_mut(),
      0,
      &mut len,
    );
  }
  assert!(
    len > 0,
    "GetTokenInformation(TokenIntegrityLevel) returned len=0"
  );

  let mut buf = vec![0u8; len as usize];
  let ok = unsafe {
    GetTokenInformation(
      token,
      TokenIntegrityLevel,
      buf.as_mut_ptr().cast(),
      len,
      &mut len,
    )
  };
  assert_ne!(
    ok,
    0,
    "GetTokenInformation(TokenIntegrityLevel) failed: {}",
    std::io::Error::last_os_error()
  );

  let tml = buf.as_ptr().cast::<TOKEN_MANDATORY_LABEL>();
  let sid = unsafe { (*tml).Label.Sid };
  assert!(!sid.is_null(), "integrity SID should be non-null");

  let subauth_count = unsafe { *GetSidSubAuthorityCount(sid) } as usize;
  assert!(
    subauth_count > 0,
    "integrity SID should have sub authorities"
  );
  let rid = unsafe { *GetSidSubAuthority(sid, (subauth_count - 1) as u32) };
  unsafe {
    CloseHandle(token);
  }
  rid
}

#[test]
fn restricted_token_spawn_does_not_inherit_inaccessible_cwd() {
  let depth: u32 = std::env::var(ENV_TEST_DEPTH)
    .ok()
    .and_then(|raw| raw.parse().ok())
    .unwrap_or(0);
  let integrity_rid = current_integrity_rid();
  if integrity_rid <= 4096 {
    // Child path: we should be running under a restricted token, and the parent-provided CWD should
    // not be accessible.
    assert!(
      integrity_rid == 0 || integrity_rid == 4096,
      "expected integrity RID to be Untrusted(0) or Low(4096); got {integrity_rid}"
    );

    let cwd = std::env::var_os(ENV_TEST_CWD).expect("missing parent restricted CWD env var");
    let err =
      std::env::set_current_dir(PathBuf::from(cwd)).expect_err("expected set_current_dir to fail");
    if let Some(code) = err.raw_os_error() {
      assert!(
        code == ERROR_ACCESS_DENIED as i32
          || code == ERROR_PATH_NOT_FOUND as i32
          || code == ERROR_FILE_NOT_FOUND as i32,
        "unexpected error for blocked set_current_dir: {err:?}"
      );
    }
    return;
  }
  if depth > 0 {
    panic!(
      "expected restricted-token child to have Low/Untrusted integrity, got RID={integrity_rid}"
    );
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

  let exe = std::env::current_exe().expect("current test exe path");
  let env = vec![
    (
      OsString::from(ENV_TEST_CWD),
      tmp.path().as_os_str().to_os_string(),
    ),
    (OsString::from(ENV_TEST_DEPTH), OsString::from("1")),
  ];

  let cfg = SpawnConfig {
    exe,
    args: vec![
      OsString::from("--exact"),
      OsString::from(TEST_NAME),
      OsString::from("--nocapture"),
    ],
    env,
    current_dir: None,
    inherit_handles: Vec::new(),
    appcontainer: None,
    job: None,
    mitigation_policy: None,
    all_application_packages_hardened: true,
  };

  let token = RestrictedToken::for_current_process_low_integrity()
    .expect("create restricted token")
    .into_handle();
  let child =
    restricted_token::spawn_with_token(&cfg, &token).expect("spawn restricted-token child");

  let exit_code = child
    .wait_timeout(Duration::from_secs(30))
    .expect("wait for child")
    .unwrap_or_else(|| {
      let _ = child.kill();
      panic!("timed out waiting for restricted-token child to exit");
    });
  assert_eq!(exit_code, 0, "child should exit successfully");
}
