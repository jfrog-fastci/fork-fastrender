use std::fs;
use std::path::PathBuf;
use std::process::Command;

use tempfile::tempdir;

fn repo_root() -> PathBuf {
  PathBuf::from(env!("CARGO_MANIFEST_DIR"))
    .parent()
    .expect("xtask crate should live under the workspace root")
    .to_path_buf()
}

#[test]
fn js_wpt_dom_smoke_writes_report_and_honors_fail_on() {
  let temp = tempdir().expect("tempdir");

  let report_pass = temp.path().join("wpt_dom_pass.json");
  let output = Command::new(env!("CARGO_BIN_EXE_xtask"))
    .current_dir(repo_root())
    .args([
      "js",
      "wpt-dom",
      "--filter",
      "smoke/sync-pass.html",
      "--report",
    ])
    .arg(&report_pass)
    .output()
    .expect("run xtask js wpt-dom (pass)");
  assert!(
    output.status.success(),
    "expected js wpt-dom (pass) to succeed.\nstdout:\n{}\nstderr:\n{}",
    String::from_utf8_lossy(&output.stdout),
    String::from_utf8_lossy(&output.stderr)
  );
  assert!(
    report_pass.is_file(),
    "expected report file to be created at {}",
    report_pass.display()
  );
  let report_json: serde_json::Value =
    serde_json::from_str(&fs::read_to_string(&report_pass).expect("read report"))
      .expect("parse report JSON");
  assert_eq!(report_json["summary"]["total"].as_u64(), Some(1));

  let report_xfail_new = temp.path().join("wpt_dom_xfail_new.json");
  let output = Command::new(env!("CARGO_BIN_EXE_xtask"))
    .current_dir(repo_root())
    .args([
      "js",
      "wpt-dom",
      "--filter",
      "smoke/sync-fail.html",
      "--fail-on",
      "new",
      "--report",
    ])
    .arg(&report_xfail_new)
    .output()
    .expect("run xtask js wpt-dom (xfail/new)");
  assert!(
    output.status.success(),
    "expected js wpt-dom (xfail/new) to succeed.\nstdout:\n{}\nstderr:\n{}",
    String::from_utf8_lossy(&output.stdout),
    String::from_utf8_lossy(&output.stderr)
  );
  assert!(
    report_xfail_new.is_file(),
    "expected report file to be created at {}",
    report_xfail_new.display()
  );

  let report_xfail_all = temp.path().join("wpt_dom_xfail_all.json");
  let output = Command::new(env!("CARGO_BIN_EXE_xtask"))
    .current_dir(repo_root())
    .args([
      "js",
      "wpt-dom",
      "--filter",
      "smoke/sync-fail.html",
      "--fail-on",
      "all",
      "--report",
    ])
    .arg(&report_xfail_all)
    .output()
    .expect("run xtask js wpt-dom (xfail/all)");
  assert!(
    !output.status.success(),
    "expected js wpt-dom (xfail/all) to fail.\nstdout:\n{}\nstderr:\n{}",
    String::from_utf8_lossy(&output.stdout),
    String::from_utf8_lossy(&output.stderr)
  );
  assert!(
    report_xfail_all.is_file(),
    "expected report file to be created at {} even when fail_on=all triggers failure",
    report_xfail_all.display()
  );
}

