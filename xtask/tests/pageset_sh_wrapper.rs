use std::path::Path;
use std::process::Command;

#[test]
fn pageset_sh_wrapper_dry_run_prints_xtask_command_with_forwarded_args() {
  let repo_root = Path::new(env!("CARGO_MANIFEST_DIR"))
    .parent()
    .expect("xtask crate should live under the repository root");

  let output = Command::new("bash")
    .arg(repo_root.join("scripts/pageset.sh"))
    .args([
      "--dry-run",
      "--jobs",
      "3",
      "--fetch-timeout",
      "11",
      "--render-timeout",
      "5",
      "--cache-dir",
      "target/cache",
      "--no-fetch",
      "--pages",
      "example.com",
      "--",
      "--disk-cache-max-age-secs",
      "0",
    ])
    .current_dir(repo_root)
    .output()
    .expect("run scripts/pageset.sh");

  assert!(
    output.status.success(),
    "expected scripts/pageset.sh --dry-run to exit 0\nstdout:\n{}\nstderr:\n{}",
    String::from_utf8_lossy(&output.stdout),
    String::from_utf8_lossy(&output.stderr)
  );

  let stdout = String::from_utf8(output.stdout).expect("stdout should be valid UTF-8");

  assert!(
    stdout.contains("cargo xtask pageset"),
    "stdout should contain `cargo xtask pageset` (got {stdout:?})"
  );
  assert!(
    stdout.contains("--jobs 3"),
    "stdout should forward --jobs (got {stdout:?})"
  );
  assert!(
    stdout.contains("--fetch-timeout 11"),
    "stdout should forward --fetch-timeout (got {stdout:?})"
  );
  assert!(
    stdout.contains("--render-timeout 5"),
    "stdout should forward --render-timeout (got {stdout:?})"
  );
  assert!(
    stdout.contains("--cache-dir target/cache"),
    "stdout should forward --cache-dir (got {stdout:?})"
  );
  assert!(
    stdout.contains("--no-fetch"),
    "stdout should forward --no-fetch (got {stdout:?})"
  );
  assert!(
    stdout.contains("--pages example.com"),
    "stdout should forward --pages (got {stdout:?})"
  );
  assert!(
    stdout.contains("-- --disk-cache-max-age-secs 0"),
    "stdout should forward extra args after `--` (got {stdout:?})"
  );
}

