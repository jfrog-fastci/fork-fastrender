use assert_cmd::Command;
use predicates::str::contains;
use std::fs;
use std::time::Duration;
use tempfile::tempdir;

const CLI_TIMEOUT: Duration = Duration::from_secs(30);

fn typecheck_cli() -> Command {
  assert_cmd::cargo::cargo_bin_cmd!("typecheck-ts-cli")
}

#[test]
fn strict_native_reports_explicit_any() {
  let tmp = tempdir().expect("temp dir");
  let entry = tmp.path().join("main.ts");
  fs::write(&entry, "let x: any = 1;\n").expect("write main.ts");

  typecheck_cli()
    .timeout(CLI_TIMEOUT)
    .args(["typecheck"])
    .arg("--strict-native")
    .arg(entry.as_os_str())
    .assert()
    .failure()
    .stdout(contains("TN0001"));
}

