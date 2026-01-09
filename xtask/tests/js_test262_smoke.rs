use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use tempfile::tempdir;

#[cfg(unix)]
fn make_executable(path: &Path) {
  use std::os::unix::fs::PermissionsExt;
  let mut perms = fs::metadata(path)
    .expect("stat stub executable")
    .permissions();
  perms.set_mode(0o755);
  fs::set_permissions(path, perms).expect("chmod stub executable");
}

#[cfg(not(unix))]
fn make_executable(_path: &Path) {}

fn repo_root() -> PathBuf {
  PathBuf::from(env!("CARGO_MANIFEST_DIR"))
    .parent()
    .expect("xtask crate should live under the workspace root")
    .to_path_buf()
}

#[test]
#[cfg(unix)]
fn js_test262_smoke_writes_report() {
  let temp = tempdir().expect("tempdir");

  // Stub out `cargo` so the test doesn't actually compile/run the ecma-rs workspace.
  let bin_dir = temp.path().join("bin");
  fs::create_dir_all(&bin_dir).expect("create stub bin dir");
  let stub_cargo = bin_dir.join("cargo");
  fs::write(
    &stub_cargo,
    r#"#!/usr/bin/env sh
set -eu

report=""
while [ "$#" -gt 0 ]; do
  case "$1" in
    --report-path) report="$2"; shift 2;;
    --report-path=*) report="${1#--report-path=}"; shift;;
    *) shift;;
  esac
done

if [ -z "$report" ]; then
  echo "stub cargo: missing --report-path argument" >&2
  exit 2
fi

mkdir -p "$(dirname "$report")"
cat > "$report" <<'JSON'
{"schema_version":1,"summary":{"total":1,"passed":1,"failed":0,"timed_out":0,"skipped":0},"results":[]}
JSON
exit 0
"#,
  )
  .expect("write stub cargo");
  make_executable(&stub_cargo);

  // Build a minimal test262-like directory so the xtask wrapper's sanity checks pass.
  let test262_dir = temp.path().join("test262");
  fs::create_dir_all(test262_dir.join("harness")).expect("create harness dir");
  fs::create_dir_all(test262_dir.join("test/language/expressions/addition"))
    .expect("create test dir");
  fs::create_dir_all(test262_dir.join("test/language/expressions/multiplication"))
    .expect("create test dir");
  fs::write(test262_dir.join("harness/assert.js"), "").expect("write assert.js");
  fs::write(test262_dir.join("harness/sta.js"), "").expect("write sta.js");
  fs::write(
    test262_dir.join("test/language/expressions/addition/a.js"),
    "let x = 1 + 1;\n",
  )
  .expect("write test file");
  fs::write(
    test262_dir.join("test/language/expressions/multiplication/b.js"),
    "let x = 2 * 3;\n",
  )
  .expect("write test file");

  let report_path = temp.path().join("out").join("test262.json");

  let path_var = std::env::var_os("PATH").unwrap_or_default();
  let mut paths = vec![bin_dir];
  paths.extend(std::env::split_paths(&path_var));
  let path = std::env::join_paths(paths).expect("join PATH");

  let output = Command::new(env!("CARGO_BIN_EXE_xtask"))
    .current_dir(repo_root())
    .env("PATH", path)
    .args([
      "js",
      "test262",
      "--suite",
      "smoke",
      "--fail-on",
      "none",
      "--test262-dir",
    ])
    .arg(&test262_dir)
    .arg("--report")
    .arg(&report_path)
    .output()
    .expect("run xtask js test262 smoke");

  assert!(
    output.status.success(),
    "expected js test262 smoke to succeed.\nstdout:\n{}\nstderr:\n{}",
    String::from_utf8_lossy(&output.stdout),
    String::from_utf8_lossy(&output.stderr)
  );

  assert!(
    report_path.is_file(),
    "expected report file to be created at {}",
    report_path.display()
  );
}

#[test]
fn js_wpt_dom_smoke_writes_report() {
  let temp = tempdir().expect("tempdir");
  let report_path = temp.path().join("wpt_dom.json");

  let output = Command::new(env!("CARGO_BIN_EXE_xtask"))
    .current_dir(repo_root())
    .args([
      "js",
      "wpt-dom",
      "--suite",
      "smoke",
      "--fail-on",
      "none",
      "--report",
    ])
    .arg(&report_path)
    .output()
    .expect("run xtask js wpt-dom smoke");

  assert!(
    output.status.success(),
    "expected js wpt-dom smoke to succeed.\nstdout:\n{}\nstderr:\n{}",
    String::from_utf8_lossy(&output.stdout),
    String::from_utf8_lossy(&output.stderr)
  );

  assert!(
    report_path.is_file(),
    "expected report file to be created at {}",
    report_path.display()
  );

  let json = fs::read_to_string(&report_path).expect("read report json");
  let value: serde_json::Value = serde_json::from_str(&json).expect("parse report json");
  let total = value
    .get("summary")
    .and_then(|summary| summary.get("total"))
    .and_then(|n| n.as_u64())
    .unwrap_or(0);
  assert!(total > 0, "expected report summary.total > 0; got {total}");
}
