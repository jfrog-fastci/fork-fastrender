use assert_cmd::Command;
use std::io::Write;
use std::time::Duration;

fn effect_js_cli() -> Command {
  assert_cmd::cargo::cargo_bin_cmd!("effect-js-cli")
}

#[test]
fn analyze_reports_known_apis_and_patterns() {
  let mut fixture = tempfile::NamedTempFile::new().unwrap();
  fixture
    .write_all(
      br#"
const xs = [1, 2, 3, 4];
const sum = xs.map(x => x * 2).filter(x => x > 2).reduce((a, b) => a + b, 0);

const fs = require("node:fs");
fs.readFile("x", () => {});

async function run(urls: string[]) {
  return Promise.all([fetch(urls[0]), fetch(urls[1])]);
}
"#,
    )
    .unwrap();

  let assert = effect_js_cli()
    .timeout(Duration::from_secs(10))
    .arg("analyze")
    .arg(fixture.path())
    .assert()
    .success()
    .code(0);

  let stdout = String::from_utf8_lossy(&assert.get_output().stdout);
  assert!(
    stdout.contains("Array.prototype.map"),
    "stdout missing Array.prototype.map:\n{stdout}"
  );
  assert!(
    stdout.contains("Array.prototype.filter"),
    "stdout missing Array.prototype.filter:\n{stdout}"
  );
  assert!(
    stdout.contains("Array.prototype.reduce"),
    "stdout missing Array.prototype.reduce:\n{stdout}"
  );
  assert!(
    stdout.contains("Promise.all"),
    "stdout missing Promise.all:\n{stdout}"
  );
  assert!(stdout.contains("fetch"), "stdout missing fetch:\n{stdout}");
  assert!(
    stdout.contains("MapFilterReduce"),
    "stdout missing MapFilterReduce:\n{stdout}"
  );
  assert!(
    stdout.contains("PromiseAllFetch"),
    "stdout missing PromiseAllFetch:\n{stdout}"
  );
  assert!(
    stdout.contains("node:fs.readFile"),
    "stdout missing node:fs.readFile:\n{stdout}"
  );
}
