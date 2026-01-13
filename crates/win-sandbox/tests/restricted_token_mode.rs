#![cfg(windows)]

use std::ffi::OsStr;
use std::ffi::OsString;
use std::os::windows::ffi::OsStrExt;
use std::path::Path;
use std::path::PathBuf;
use std::time::Duration;

use win_sandbox::{mitigations, restricted_token, RestrictedToken, SpawnConfig};
use windows_sys::Win32::Foundation::{
  CloseHandle, LocalFree, ERROR_ACCESS_DENIED, ERROR_FILE_NOT_FOUND, ERROR_PATH_NOT_FOUND, HANDLE,
};
use windows_sys::Win32::Security::Authorization::{
  ConvertStringSidToSidW, SetEntriesInAclW, SetNamedSecurityInfoW, EXPLICIT_ACCESS_W, GRANT_ACCESS,
  NO_MULTIPLE_TRUSTEE, SE_FILE_OBJECT, TRUSTEE_IS_SID, TRUSTEE_IS_UNKNOWN, TRUSTEE_W,
};
use windows_sys::Win32::Security::{
  GetSidSubAuthority, GetSidSubAuthorityCount, GetTokenInformation, TokenIntegrityLevel,
  NO_INHERITANCE, PSID, TOKEN_MANDATORY_LABEL, TOKEN_QUERY,
};
use windows_sys::Win32::System::Threading::GetCurrentProcess;

const TEST_NAME: &str = "restricted_token_spawn_enforces_low_integrity_and_blocks_userprofile";
const ENV_TEST_FILE: &str = "WIN_SANDBOX_TEST_USERPROFILE_FILE";
const ENV_TEST_DEPTH: &str = "WIN_SANDBOX_TEST_DEPTH";

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
  // group, so access checks should fail even though the caller is the same user.
  const USERS_SID: &str = "S-1-5-32-545";
  // Generic access rights from winnt.h.
  const GENERIC_ALL: u32 = 0x1000_0000;
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

  // Ensure we free the SID allocated by `ConvertStringSidToSidW`.
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
fn restricted_token_spawn_enforces_low_integrity_and_blocks_userprofile() {
  const DISABLE_MITIGATIONS_ENV: &str = "FASTR_DISABLE_WIN_MITIGATIONS";
  let prev_disable_mitigations = std::env::var_os(DISABLE_MITIGATIONS_ENV);
  std::env::remove_var(DISABLE_MITIGATIONS_ENV);
  struct EnvRestore(Option<OsString>);
  impl Drop for EnvRestore {
    fn drop(&mut self) {
      match self.0.take() {
        Some(value) => std::env::set_var(DISABLE_MITIGATIONS_ENV, value),
        None => std::env::remove_var(DISABLE_MITIGATIONS_ENV),
      }
    }
  }
  let _restore = EnvRestore(prev_disable_mitigations);

  let depth: u32 = std::env::var(ENV_TEST_DEPTH)
    .ok()
    .and_then(|raw| raw.parse().ok())
    .unwrap_or(0);
  let integrity_rid = current_integrity_rid();
  if integrity_rid <= 4096 {
    // Child-side assertions (we're running under the restricted token).
    assert!(
      integrity_rid == 0 || integrity_rid == 4096,
      "expected integrity RID to be Untrusted(0) or Low(4096); got {integrity_rid}"
    );

    mitigations::verify_renderer_mitigations_current_process()
      .expect("child mitigation verification");

    let path = std::env::var_os(ENV_TEST_FILE).expect("missing userprofile test file path");

    // Attempt to read a file in USERPROFILE; should fail under the restricted token.
    let err = std::fs::read(PathBuf::from(path)).expect_err("expected file read to fail");
    if let Some(code) = err.raw_os_error() {
      assert!(
        code == ERROR_ACCESS_DENIED as i32
          || code == ERROR_PATH_NOT_FOUND as i32
          || code == ERROR_FILE_NOT_FOUND as i32,
        "unexpected error for blocked file read: {err:?}"
      );
    }
    return;
  }
  if depth > 0 {
    panic!(
      "expected restricted-token child to have Low/Untrusted integrity, got RID={integrity_rid}"
    );
  }

  let exe = std::env::current_exe().expect("current test exe path");

  let userprofile = std::env::var_os("USERPROFILE").expect("USERPROFILE should be set on Windows");
  let userprofile = PathBuf::from(userprofile);
  let unique = format!(
    "win-sandbox-test-{}-{}.txt",
    std::process::id(),
    std::time::SystemTime::now()
      .duration_since(std::time::UNIX_EPOCH)
      .expect("SystemTime monotonic")
      .as_nanos()
  );
  let file_path = userprofile.join(unique);
  std::fs::write(&file_path, b"win-sandbox restricted token smoke")
    .expect("write test file in USERPROFILE");

  // Override the file DACL so access is granted via the `Users` group only. The restricted token
  // disables `Users`, so the child should be denied even though it's the same user account.
  set_users_only_dacl(&file_path).expect("set Users-only DACL on test file");
  assert!(
    std::fs::read(&file_path).is_ok(),
    "parent should be able to read the file it created"
  );

  let env = vec![
    (
      OsString::from(ENV_TEST_FILE),
      file_path.as_os_str().to_os_string(),
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
    mitigation_policy: Some(mitigations::renderer_mitigation_policy()),
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

  let _ = std::fs::remove_file(&file_path);
}
