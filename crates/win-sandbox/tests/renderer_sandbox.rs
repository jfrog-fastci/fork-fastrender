#![cfg(windows)]

mod common;

use std::ffi::OsString;
use std::path::PathBuf;
use std::process::Command;

use tempfile::tempdir;

use win_sandbox::renderer::RendererSandbox;
use win_sandbox::AppContainerProfile;

use windows_sys::Win32::Foundation::LocalFree;
use windows_sys::Win32::Security::Authorization::ConvertSidToStringSidW;

const DISABLE_MITIGATIONS_ENV: &str = "FASTR_DISABLE_WIN_MITIGATIONS";

struct EnvVarRestore {
  key: &'static str,
  prev: Option<OsString>,
}

impl EnvVarRestore {
  fn remove(key: &'static str) -> Self {
    let prev = std::env::var_os(key);
    std::env::remove_var(key);
    Self { key, prev }
  }
}

impl Drop for EnvVarRestore {
  fn drop(&mut self) {
    match self.prev.take() {
      Some(value) => std::env::set_var(self.key, value),
      None => std::env::remove_var(self.key),
    }
  }
}

fn icacls_grant_rx(path: &std::path::Path, sid: &str, inherit: bool) {
  let mut grant = OsString::from(sid);
  if inherit {
    grant.push(":(OI)(CI)(RX)");
  } else {
    grant.push(":(RX)");
  }

  let output = Command::new("icacls")
    .arg(path)
    .arg("/grant")
    .arg(grant)
    .output()
    .expect("failed to run icacls");

  if !output.status.success() {
    panic!(
      "icacls failed: status={:?}\nstdout:\n{}\nstderr:\n{}",
      output.status,
      String::from_utf8_lossy(&output.stdout),
      String::from_utf8_lossy(&output.stderr)
    );
  }
}

fn sid_to_string(sid: windows_sys::Win32::Security::PSID) -> String {
  let mut wide: *mut u16 = std::ptr::null_mut();
  let ok = unsafe { ConvertSidToStringSidW(sid, &mut wide) };
  assert_ne!(
    ok,
    0,
    "ConvertSidToStringSidW failed: {}",
    std::io::Error::last_os_error()
  );
  assert!(!wide.is_null(), "ConvertSidToStringSidW returned null");

  unsafe {
    let mut len = 0usize;
    while *wide.add(len) != 0 {
      len += 1;
    }
    let slice = std::slice::from_raw_parts(wide, len);
    let s = String::from_utf16_lossy(slice);
    LocalFree(wide as _);
    s
  }
}

#[test]
fn renderer_sandbox_spawns_appcontainer_job_and_blocks_grandchildren() {
  let _mitigation_guard = EnvVarRestore::remove(DISABLE_MITIGATIONS_ENV);

  if !common::require_full_sandbox_support(
    "renderer_sandbox_spawns_appcontainer_job_and_blocks_grandchildren",
  ) {
    return;
  }

  // Cargo places the probe in a directory that is typically not readable/executable by arbitrary
  // AppContainer processes. Copy it into a temp directory we can ACL for AppContainer access.
  let probe_src = PathBuf::from(env!("CARGO_BIN_EXE_renderer_sandbox_probe"));
  let tmp = tempdir().expect("tempdir");

  // Grant read/execute to the derived AppContainer SID. This remains correct even when the sandbox
  // hardens the token by removing `ALL APPLICATION PACKAGES` (S-1-15-2-1).
  let profile = AppContainerProfile::ensure(
    "FastRender.Renderer",
    "FastRender Renderer",
    "FastRender renderer AppContainer profile",
  )
  .expect("ensure AppContainer profile");
  let sid_str = sid_to_string(profile.sid().as_ptr());
  let sid_grant = format!("*{sid_str}");

  icacls_grant_rx(tmp.path(), &sid_grant, true);

  let probe_dst = tmp
    .path()
    .join(probe_src.file_name().expect("probe file name"));
  std::fs::copy(&probe_src, &probe_dst).expect("copy probe");

  // Ensure the file itself also has the ACE (in case inheritance is disabled on this host).
  icacls_grant_rx(&probe_dst, &sid_grant, false);

  let sandbox = RendererSandbox::new_default().expect("create RendererSandbox");
  let mut child = sandbox
    .spawn(probe_dst, vec![], vec![], vec![])
    .expect("spawn sandboxed probe");

  let code = child.wait().expect("wait for child");
  assert_eq!(code, 0, "probe exited with non-zero status: {code}");
}
