use assert_cmd::Command;
use std::io::Write;
use std::path::PathBuf;
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

  let kb_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
    .join("../knowledge-base")
    .canonicalize()
    .expect("resolve knowledge-base dir");

  let assert = effect_js_cli()
    .timeout(Duration::from_secs(10))
    .arg("--kb-dir")
    .arg(kb_dir)
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

#[test]
fn analyze_reports_semantic_signals_when_enabled() {
  let mut fixture = tempfile::NamedTempFile::new().unwrap();
  fixture
    .write_all(
      br#"
const xs = [1, 2, 3, 4];
const sum = xs.map(x => x * 2).filter(x => x > 2).reduce((a, b) => a + b, 0);

async function run(urls: string[]) {
  return Promise.all([fetch(urls[0]), fetch(urls[1])]);
}
"#,
    )
    .unwrap();

  let kb_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
    .join("../knowledge-base")
    .canonicalize()
    .expect("resolve knowledge-base dir");

  let assert = effect_js_cli()
    .timeout(Duration::from_secs(10))
    .arg("--kb-dir")
    .arg(kb_dir)
    .arg("analyze")
    .arg("--signals")
    .arg(fixture.path())
    .assert()
    .success()
    .code(0);

  let stdout = String::from_utf8_lossy(&assert.get_output().stdout);
  assert!(
    stdout.contains("== Semantic Signals =="),
    "stdout missing semantic signals header:\n{stdout}"
  );
  assert!(
    stdout.contains("AsyncFunctionWithoutAwait"),
    "stdout missing AsyncFunctionWithoutAwait:\n{stdout}"
  );
  assert!(
    stdout.contains("ConstBinding"),
    "stdout missing ConstBinding:\n{stdout}"
  );
}
