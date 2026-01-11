#![cfg(target_os = "linux")]

use assert_cmd::Command;
use std::time::Duration;

fn phdr_smoke_bin() -> Command {
  assert_cmd::cargo::cargo_bin_cmd!("linux_phdr_stackmaps_smoke")
}

#[test]
fn discovers_stackmaps_via_dl_iterate_phdr_without_linker_script() {
  let assert = phdr_smoke_bin()
    .timeout(Duration::from_secs(5))
    .assert()
    .success();

  let stdout = String::from_utf8_lossy(&assert.get_output().stdout);
  let mut len: Option<usize> = None;
  let mut callsites: Option<usize> = None;
  for part in stdout.split_whitespace() {
    if let Some(v) = part.strip_prefix("LEN=") {
      len = Some(v.parse().expect("LEN should be an integer"));
    } else if let Some(v) = part.strip_prefix("CALLSITES=") {
      callsites = Some(v.parse().expect("CALLSITES should be an integer"));
    }
  }

  let len = len.unwrap_or_else(|| panic!("missing LEN in stdout:\n{stdout}"));
  let callsites = callsites.unwrap_or_else(|| panic!("missing CALLSITES in stdout:\n{stdout}"));

  assert!(len > 0, "expected non-empty stackmaps, stdout:\n{stdout}");
  assert_eq!(callsites, 1, "stdout:\n{stdout}");
}

