use assert_cmd::Command;
use std::time::Duration;

fn stackmap_dump() -> Command {
  assert_cmd::cargo::cargo_bin_cmd!("stackmap-dump")
}

#[test]
fn summary_smoke() {
  let fixture = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
    .join("tests")
    .join("fixtures")
    .join("simple_stackmap.bin");

  let assert = stackmap_dump()
    .timeout(Duration::from_secs(5))
    .arg("--summary")
    .arg(&fixture)
    .assert()
    .success();

  let stdout = String::from_utf8_lossy(&assert.get_output().stdout);
  assert!(stdout.contains("StackMap v3"), "stdout was:\n{stdout}");
  assert!(stdout.contains("functions: 1"), "stdout was:\n{stdout}");
  assert!(stdout.contains("records: 1"), "stdout was:\n{stdout}");
  assert!(
    stdout.contains("addr=0x0000000000001000"),
    "stdout was:\n{stdout}"
  );
}

#[test]
fn records_json_smoke() {
  let fixture = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
    .join("tests")
    .join("fixtures")
    .join("simple_stackmap.bin");

  let assert = stackmap_dump()
    .timeout(Duration::from_secs(5))
    .arg("--records")
    .arg("--json")
    .arg(&fixture)
    .assert()
    .success();

  let v: serde_json::Value =
    serde_json::from_slice(&assert.get_output().stdout).expect("stdout should be valid JSON");
  assert_eq!(v["mode"], "records");
  assert_eq!(v["version"], 3);
  assert_eq!(v["records"].as_array().unwrap().len(), 1);
  assert_eq!(v["records"][0]["callsite_address"], "0x0000000000001010");
  assert_eq!(v["records"][0]["locations"].as_array().unwrap().len(), 2);
}
