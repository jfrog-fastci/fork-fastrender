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

#[cfg(unix)]
fn bash_version_major() -> Option<u32> {
  let output = Command::new("bash")
    .args(["-c", "echo ${BASH_VERSINFO[0]}"])
    .output()
    .ok()?;
  if !output.status.success() {
    return None;
  }
  String::from_utf8(output.stdout)
    .ok()
    .and_then(|s| s.trim().parse::<u32>().ok())
}

#[cfg(unix)]
fn has_bash4() -> bool {
  bash_version_major().is_some_and(|major| major >= 4)
}

#[test]
#[cfg(unix)]
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
  if !has_bash4() {
    eprintln!(
      "skipping wrapper test: bash >= 4 is required (found {:?})",
      bash_version_major()
    );
    return;
  }

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

#[test]
#[cfg(unix)]
fn wrapper_resolves_fixture_globs() {
  if !has_bash4() {
    eprintln!(
      "skipping wrapper test: bash >= 4 is required (found {:?})",
      bash_version_major()
    );
    return;
  }

  let temp = tempdir().expect("tempdir");
  let bin_dir = temp.path().join("bin");
  fs::create_dir_all(&bin_dir).expect("create stub bin dir");

  let fixtures_dir = temp.path().join("fixtures");
  fs::create_dir_all(fixtures_dir.join("a")).expect("create fixture a dir");
  fs::create_dir_all(fixtures_dir.join("ab")).expect("create fixture ab dir");
  fs::create_dir_all(fixtures_dir.join("b")).expect("create fixture b dir");
  for name in ["a", "ab", "b"] {
    fs::write(
      fixtures_dir.join(name).join("index.html"),
      "<!doctype html><title>fixture</title>",
    )
    .expect("write fixture index.html");
  }

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
      "--fixtures-dir",
      fixtures_dir.to_string_lossy().as_ref(),
      "--out-dir",
      out_dir.to_string_lossy().as_ref(),
      "a*",
    ])
    .output()
    .expect("run chrome_vs_fastrender_fixtures.sh with glob pattern");

  assert!(
    output.status.success(),
    "expected wrapper invocation to exit 0.\nstdout:\n{}\nstderr:\n{}",
    String::from_utf8_lossy(&output.stdout),
    String::from_utf8_lossy(&output.stderr)
  );

  let stdout = String::from_utf8_lossy(&output.stdout);
  assert!(
    stdout.contains("--fixtures a,ab") || stdout.contains("--fixtures ab,a"),
    "expected wrapper to expand glob into fixture list; got:\n{stdout}"
  );
}

#[test]
#[cfg(unix)]
fn wrapper_infers_out_dir_from_legacy_chrome_out_dir() {
  if !has_bash4() {
    eprintln!(
      "skipping wrapper test: bash >= 4 is required (found {:?})",
      bash_version_major()
    );
    return;
  }

  let temp = tempdir().expect("tempdir");
  let bin_dir = temp.path().join("bin");
  fs::create_dir_all(&bin_dir).expect("create stub bin dir");

  let out_dir = temp.path().join("out");
  let chrome_out_dir = out_dir.join("chrome");
  fs::create_dir_all(&chrome_out_dir).expect("create chrome out dir");

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

  let output = Command::new("bash")
    .current_dir(repo_root())
    .env("PATH", path)
    .args([
      "scripts/chrome_vs_fastrender_fixtures.sh",
      "--diff-only",
      "--chrome-out-dir",
      chrome_out_dir.to_string_lossy().as_ref(),
    ])
    .output()
    .expect("run chrome_vs_fastrender_fixtures.sh with legacy chrome out dir");

  assert!(
    output.status.success(),
    "expected wrapper invocation to exit 0.\nstdout:\n{}\nstderr:\n{}",
    String::from_utf8_lossy(&output.stdout),
    String::from_utf8_lossy(&output.stderr)
  );

  let stdout = String::from_utf8_lossy(&output.stdout);
  let out_dir_str = out_dir.to_string_lossy();
  assert!(
    stdout.contains(&format!("--out-dir {out_dir_str}")),
    "expected wrapper to infer --out-dir from --chrome-out-dir; got:\n{stdout}"
  );
}
