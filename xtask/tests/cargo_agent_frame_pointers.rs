#[cfg(unix)]
mod unix_tests {
  use std::fs;
  use std::os::unix::fs::PermissionsExt;
  use std::path::{Path, PathBuf};
  use std::process::Command;

  use tempfile::tempdir;

  fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
      .parent()
      .expect("xtask crate should live under the workspace root")
      .to_path_buf()
  }

  fn write_executable(path: &Path, contents: &str) {
    fs::write(path, contents).unwrap_or_else(|e| panic!("write {}: {e}", path.display()));
    let mut perms = fs::metadata(path)
      .unwrap_or_else(|e| panic!("stat {}: {e}", path.display()))
      .permissions();
    perms.set_mode(0o755);
    fs::set_permissions(path, perms).unwrap_or_else(|e| panic!("chmod {}: {e}", path.display()));
  }

  fn run_cargo_agent_with_stub(
    repo: &Path,
    script: &Path,
    args: &[&str],
    stub: &str,
  ) -> (String, String) {
    let temp = tempdir().expect("tempdir");
    let bin_dir = temp.path().join("bin");
    fs::create_dir_all(&bin_dir).expect("create bin dir");

    let cargo_stub = bin_dir.join("cargo");
    write_executable(&cargo_stub, stub);

    let original_path = std::env::var("PATH").unwrap_or_default();
    let path = format!("{}:{}", bin_dir.display(), original_path);

    let output = Command::new("bash")
      .current_dir(repo)
      .arg(script)
      .args(args)
      .env("PATH", path)
      // Avoid slot-locking (flock) and RLIMIT machinery in this test. We only care about argv/env
      // rewriting, and the stub cargo exits immediately.
      .env("FASTR_CARGO_SLOT", "0")
      .env("FASTR_CARGO_LIMIT_AS", "off")
      .env_remove("RUSTFLAGS")
      .output()
      .unwrap_or_else(|e| panic!("run {}: {e}", script.display()));

    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();

    assert!(
      output.status.success(),
      "expected {} to exit 0.\nstdout:\n{stdout}\nstderr:\n{stderr}",
      script.display()
    );

    (stdout, stderr)
  }

  #[test]
  fn cargo_agent_runtime_native_injects_frame_pointers_unless_allow_omit_feature_is_set() {
    let repo = repo_root();

    let scripts = [
      repo.join("scripts/cargo_agent.sh"),
      repo.join("vendor/ecma-rs/scripts/cargo_agent.sh"),
    ];

    for script in scripts {
      let (stdout, _stderr) = run_cargo_agent_with_stub(
        &repo,
        &script,
        &["check", "-p", "runtime-native"],
        r#"#!/usr/bin/env bash
set -euo pipefail
echo "RUSTFLAGS=${RUSTFLAGS-}"
"#,
      );
      assert!(
        stdout.contains("force-frame-pointers=yes"),
        "expected {} to inject frame pointers for runtime-native.\nstdout:\n{stdout}",
        script.display()
      );

      let (stdout, _stderr) = run_cargo_agent_with_stub(
        &repo,
        &script,
        &[
          "check",
          "-p",
          "runtime-native",
          "--features",
          "allow_omit_frame_pointers",
        ],
        r#"#!/usr/bin/env bash
set -euo pipefail
echo "RUSTFLAGS=${RUSTFLAGS-}"
"#,
      );
      assert!(
        !stdout.contains("force-frame-pointers=yes"),
        "expected {} to respect allow_omit_frame_pointers and avoid injecting frame pointers.\nstdout:\n{stdout}",
        script.display()
      );
    }
  }

  #[test]
  fn cargo_agent_autoscopes_vendor_ecma_rs_even_when_arg_after_delimiter_mentions_manifest_path() {
    let repo = repo_root();

    let scripts = [
      repo.join("scripts/cargo_agent.sh"),
      repo.join("vendor/ecma-rs/scripts/cargo_agent.sh"),
    ];

    // Regression test: the repo-root cargo wrapper auto-scopes `-p <crate>` invocations to the
    // nested `vendor/ecma-rs` workspace. It must ignore `--manifest-path` strings that appear after
    // Cargo's `--` delimiter (those are forwarded to rustc/the test harness/the executed binary).
    for script in scripts {
      let (stdout, _stderr) = run_cargo_agent_with_stub(
        &repo,
        &script,
        &[
          "check",
          "-p",
          "parse-js",
          "--",
          "--manifest-path",
          "ignored",
        ],
        r#"#!/usr/bin/env bash
set -euo pipefail
echo "PWD=$(pwd -P)"
"#,
      );
      assert!(
        stdout.contains("vendor/ecma-rs"),
        "expected {} to run cargo from vendor/ecma-rs even when `--manifest-path` appears after `--`.\nstdout:\n{stdout}",
        script.display()
      );
    }
  }
}
