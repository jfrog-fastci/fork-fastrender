#![cfg(windows)]

use std::ffi::OsString;
use std::path::PathBuf;
use std::process::Command;

use tempfile::tempdir;

use win_sandbox::renderer::RendererSandbox;

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

#[test]
fn renderer_sandbox_spawns_appcontainer_job_and_blocks_grandchildren() {
  // Cargo places the probe in a directory that is typically not readable/executable by arbitrary
  // AppContainer processes. Copy it into a temp directory we can ACL for AppContainer access.
  let probe_src = PathBuf::from(env!("CARGO_BIN_EXE_renderer_sandbox_probe"));
  let tmp = tempdir().expect("tempdir");

  // Grant read/execute to:
  // - ALL APPLICATION PACKAGES (S-1-15-2-1)
  // - ALL RESTRICTED APPLICATION PACKAGES (S-1-15-2-2)
  //
  // This makes the directory (and the copied probe) executable from within an AppContainer.
  icacls_grant_rx(tmp.path(), "*S-1-15-2-1", true);
  icacls_grant_rx(tmp.path(), "*S-1-15-2-2", true);

  let probe_dst = tmp
    .path()
    .join(probe_src.file_name().expect("probe file name"));
  std::fs::copy(&probe_src, &probe_dst).expect("copy probe");

  // Ensure the file itself also has the ACEs (in case inheritance is disabled on this host).
  icacls_grant_rx(&probe_dst, "*S-1-15-2-1", false);
  icacls_grant_rx(&probe_dst, "*S-1-15-2-2", false);

  let sandbox = RendererSandbox::new_default().expect("create RendererSandbox");
  let mut child = sandbox
    .spawn(probe_dst, vec![], vec![], vec![])
    .expect("spawn sandboxed probe");

  let code = child.wait().expect("wait for child");
  assert_eq!(code, 0, "probe exited with non-zero status: {code}");
}
