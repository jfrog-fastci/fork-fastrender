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
fn wrapper_help_runs() {
  let output = Command::new("bash")
    .current_dir(repo_root())
    .args(["scripts/chrome_vs_fastrender_fixtures.sh", "--help"])
    .output()
    .expect("run chrome_vs_fastrender_fixtures.sh --help");

  assert!(
    output.status.success(),
    "expected --help to exit 0.\nstdout:\n{}\nstderr:\n{}",
    String::from_utf8_lossy(&output.stdout),
    String::from_utf8_lossy(&output.stderr)
  );

  let stdout = String::from_utf8_lossy(&output.stdout);
  assert!(
    stdout.contains("usage: scripts/chrome_vs_fastrender_fixtures.sh"),
    "expected wrapper help to mention usage; got:\n{stdout}"
  );
}

#[test]
#[cfg(unix)]
fn wrapper_diff_only_invokes_fixture_chrome_diff() {
  let temp = tempdir().expect("tempdir");
  let bin_dir = temp.path().join("bin");
  fs::create_dir_all(&bin_dir).expect("create stub bin dir");

  let stub_cargo = bin_dir.join("cargo");
  fs::write(
    &stub_cargo,
    r#"#!/usr/bin/env sh
set -eu
printf 'stub cargo invoked:'
for arg in "$@"; do
  printf ' %s' "$arg"
done
printf '\n'
exit 0
"#,
  )
  .expect("write stub cargo");
  make_executable(&stub_cargo);

  let path_var = std::env::var_os("PATH").unwrap_or_default();
  let mut paths = vec![bin_dir];
  paths.extend(std::env::split_paths(&path_var));
  let path = std::env::join_paths(paths).expect("join PATH");

  let out_dir = temp.path().join("out");

  let output = Command::new("bash")
    .current_dir(repo_root())
    .env("PATH", path)
    .args([
      "scripts/chrome_vs_fastrender_fixtures.sh",
      "--diff-only",
      "--out-dir",
      out_dir.to_string_lossy().as_ref(),
    ])
    .output()
    .expect("run chrome_vs_fastrender_fixtures.sh --diff-only");

  assert!(
    output.status.success(),
    "expected --diff-only wrapper invocation to exit 0.\nstdout:\n{}\nstderr:\n{}",
    String::from_utf8_lossy(&output.stdout),
    String::from_utf8_lossy(&output.stderr)
  );

  let stdout = String::from_utf8_lossy(&output.stdout);
  assert!(
    stdout.contains("fixture-chrome-diff") && stdout.contains("--diff-only"),
    "expected wrapper to invoke `cargo xtask fixture-chrome-diff --diff-only`; got:\n{stdout}"
  );
}

