use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use tempfile::tempdir;

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

fn write_stub_cargo(dir: &Path) -> PathBuf {
  let path = dir.join("cargo");
  // A stub `cargo` binary that records argv into $CARGO_STUB_OUT (one arg per line) and exits 0.
  //
  // This keeps the wrapper test fast and deterministic: we only want to validate argument parsing
  // without invoking a real build or Chrome.
  let script = r#"#!/usr/bin/env bash
set -euo pipefail

out="${CARGO_STUB_OUT:-}"
if [[ -n "${out}" ]]; then
  printf '%s\n' "$@" > "${out}"
fi

exit 0
"#;

  fs::write(&path, script).expect("write stub cargo");
  #[cfg(unix)]
  {
    let mut perms = fs::metadata(&path).expect("stat stub cargo").permissions();
    perms.set_mode(0o755);
    fs::set_permissions(&path, perms).expect("chmod stub cargo");
  }
  path
}

#[test]
#[cfg(unix)]
fn chrome_fixture_baseline_script_parses_flags_without_treating_them_as_fixture_patterns() {
  let tmp = tempdir().expect("tempdir");
  let fixtures_dir = tmp.path().join("fixtures");
  let out_dir = tmp.path().join("out");
  let bin_dir = tmp.path().join("bin");
  fs::create_dir_all(&fixtures_dir).expect("create fixtures dir");
  fs::create_dir_all(&out_dir).expect("create out dir");
  fs::create_dir_all(&bin_dir).expect("create bin dir");

  // Minimal fixture structure expected by the wrapper: <fixtures>/<name>/index.html.
  let fixture_name = "fixture_one";
  let fixture_root = fixtures_dir.join(fixture_name);
  fs::create_dir_all(&fixture_root).expect("create fixture root");
  fs::write(
    fixture_root.join("index.html"),
    "<!doctype html><meta charset=utf-8><p>fixture</p>",
  )
  .expect("write fixture index.html");

  let record_path = tmp.path().join("cargo_args.json");
  write_stub_cargo(&bin_dir);

  let repo_root = Path::new(env!("CARGO_MANIFEST_DIR"));
  let script_path = repo_root.join("scripts/chrome_fixture_baseline.sh");
  let existing_path = std::env::var("PATH").unwrap_or_default();
  let path_env = format!("{}:{}", bin_dir.to_string_lossy(), existing_path);

  let output = Command::new(&script_path)
    .env("PATH", &path_env)
    .env("CARGO_STUB_OUT", &record_path)
    .args([
      "--fixtures-dir",
      fixtures_dir.to_str().unwrap(),
      "--out-dir",
      out_dir.to_str().unwrap(),
      "--viewport",
      "200x150",
      "--dpr",
      "1.25",
      "--timeout",
      "5",
      "--",
      fixture_name,
    ])
    .output()
    .expect("run scripts/chrome_fixture_baseline.sh");

  assert!(
    output.status.success(),
    "expected scripts/chrome_fixture_baseline.sh to succeed, got status={:?}\nstdout:\n{}\nstderr:\n{}",
    output.status.code(),
    String::from_utf8_lossy(&output.stdout),
    String::from_utf8_lossy(&output.stderr),
  );

  let recorded = fs::read_to_string(&record_path).expect("read record");
  let args: Vec<&str> = recorded.lines().collect();
  assert!(
    args.len() >= 2,
    "expected stub cargo to record at least the subcommand, got:\n{recorded}"
  );
  assert_eq!(args[0], "xtask");
  assert_eq!(args[1], "chrome-baseline-fixtures");

  assert!(
    args.contains(&"--viewport") && args.contains(&"200x150"),
    "expected stub cargo args to include viewport, got:\n{recorded}"
  );
  assert!(
    args.contains(&"--dpr") && args.contains(&"1.25"),
    "expected stub cargo args to include dpr, got:\n{recorded}"
  );
  assert!(
    args.contains(&"--") && args.contains(&fixture_name),
    "expected stub cargo args to include fixture list after --, got:\n{recorded}"
  );
}

#[test]
#[cfg(unix)]
fn chrome_fixture_baseline_script_errors_on_unknown_flag() {
  let repo_root = Path::new(env!("CARGO_MANIFEST_DIR"));
  let script_path = repo_root.join("scripts/chrome_fixture_baseline.sh");

  let output = Command::new(&script_path)
    .arg("--definitely-not-a-flag")
    .output()
    .expect("run scripts/chrome_fixture_baseline.sh");

  assert!(
    !output.status.success(),
    "expected non-zero exit for unknown flag, got {:?}",
    output.status.code()
  );
  let stderr = String::from_utf8_lossy(&output.stderr);
  assert!(
    stderr.contains("unknown option"),
    "expected stderr to mention unknown option, got:\n{stderr}"
  );
}
