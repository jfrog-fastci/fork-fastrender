#![cfg(feature = "with-node")]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use typecheck_ts_harness::tsc::node_available;

fn node_or_skip(context: &str) -> Option<PathBuf> {
  let node_path = PathBuf::from("node");
  if !node_available(&node_path) {
    eprintln!("skipping {context}: node not available");
    return None;
  }
  Some(node_path)
}

/// Spawn Node with a near-empty environment.
///
/// We still forward enough variables to make process launching work on common
/// platforms (notably `PATH` so `node` can be resolved).
fn node_command(node_path: &Path) -> Command {
  let mut cmd = Command::new(node_path);
  cmd.env_clear();

  if let Some(path) = std::env::var_os("PATH") {
    cmd.env("PATH", path);
  }
  if let Some(pathext) = std::env::var_os("PATHEXT") {
    cmd.env("PATHEXT", pathext);
  }
  if let Some(system_root) = std::env::var_os("SYSTEMROOT") {
    cmd.env("SYSTEMROOT", system_root);
  }
  if let Some(windir) = std::env::var_os("WINDIR") {
    cmd.env("WINDIR", windir);
  }
  if let Some(tmp) = std::env::var_os("TMP") {
    cmd.env("TMP", tmp);
  }
  if let Some(temp) = std::env::var_os("TEMP") {
    cmd.env("TEMP", temp);
  }

  cmd
}

fn copy_script(src_root: &Path, dst_root: &Path, name: &str) {
  let src = src_root.join("scripts").join(name);
  let dst = dst_root.join("scripts").join(name);
  fs::create_dir_all(dst.parent().expect("scripts dir")).expect("create scripts dir");
  fs::copy(&src, &dst).expect("copy script");
}

fn write_stub_typescript(pkg_dir: &Path, version: &str) {
  fs::create_dir_all(pkg_dir).expect("create typescript package dir");
  fs::write(
    pkg_dir.join("package.json"),
    format!("{{\"name\":\"typescript\",\"main\":\"index.js\"}}\n"),
  )
  .expect("write typescript package.json");
  fs::write(
    pkg_dir.join("index.js"),
    format!("module.exports = {{ version: \"{version}\" }};\n"),
  )
  .expect("write typescript index.js");
}

#[test]
fn probe_errors_without_local_typescript_even_if_other_fallbacks_exist() {
  let node_path = match node_or_skip("typescript_loader missing typescript") {
    Some(path) => path,
    None => return,
  };

  let tmp = tempfile::tempdir().expect("tempdir");

  // Set up an isolated harness checkout with scripts but *without*
  // `typecheck-ts-harness/node_modules/typescript`.
  let harness_root = tmp.path().join("typecheck-ts-harness");
  fs::create_dir_all(harness_root.join("scripts")).expect("create harness scripts dir");
  fs::write(harness_root.join("package.json"), "{\"private\":true}\n").expect("write package.json");
  let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
  copy_script(manifest_dir, &harness_root, "typescript_loader.js");
  copy_script(manifest_dir, &harness_root, "typescript_probe.js");

  // Fake global `typescript` via NODE_PATH. The loader must *not* fall back to it.
  let global_modules = tmp.path().join("global_node_modules");
  write_stub_typescript(&global_modules.join("typescript"), "0.0.0-global");

  // Fake the legacy parse-js/tests/TypeScript fallback. The loader must ignore it.
  let ts_submodule_root = tmp.path().join("parse-js").join("tests").join("TypeScript");
  fs::create_dir_all(&ts_submodule_root).expect("create fake ts submodule");
  fs::write(ts_submodule_root.join("package.json"), "{\"private\":true}\n")
    .expect("write fake ts submodule package.json");
  write_stub_typescript(
    &ts_submodule_root.join("node_modules").join("typescript"),
    "0.0.0-submodule",
  );

  let probe = harness_root.join("scripts").join("typescript_probe.js");
  let output = node_command(&node_path)
    .arg(&probe)
    .env("NODE_PATH", &global_modules)
    .output()
    .expect("run typescript_probe.js");

  assert!(
    !output.status.success(),
    "expected probe failure; stdout: {}\nstderr: {}",
    String::from_utf8_lossy(&output.stdout),
    String::from_utf8_lossy(&output.stderr)
  );

  let stderr = String::from_utf8_lossy(&output.stderr);
  assert!(
    stderr.contains("Cannot load the TypeScript compiler (`typescript` npm package)."),
    "stderr was: {stderr}"
  );
  assert!(stderr.contains("cd typecheck-ts-harness && npm ci"), "stderr was: {stderr}");
  assert!(
    stderr.contains("TYPECHECK_TS_HARNESS_TYPESCRIPT_DIR"),
    "stderr was: {stderr}"
  );
  assert!(stderr.contains("Load attempts:"), "stderr was: {stderr}");
  assert!(
    stderr.contains("- typecheck-ts-harness/package.json ("),
    "stderr was: {stderr}"
  );

  // These nondeterministic fallbacks must no longer appear in the attempt list or
  // message text.
  assert!(
    !stderr.contains("parse-js/tests/TypeScript"),
    "unexpected parse-js fallback mention in: {stderr}"
  );
  assert!(
    !stderr.contains("require('typescript')"),
    "unexpected global require mention in: {stderr}"
  );
  assert!(
    !stderr.contains("default Node resolution"),
    "unexpected global require mention in: {stderr}"
  );
}

#[test]
fn probe_loads_typescript_from_typescript_dir_env_override() {
  let node_path = match node_or_skip("typescript_loader env override") {
    Some(path) => path,
    None => return,
  };

  let tmp = tempfile::tempdir().expect("tempdir");

  let harness_root = tmp.path().join("typecheck-ts-harness");
  fs::create_dir_all(harness_root.join("scripts")).expect("create harness scripts dir");
  fs::write(harness_root.join("package.json"), "{\"private\":true}\n").expect("write package.json");
  let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
  copy_script(manifest_dir, &harness_root, "typescript_loader.js");
  copy_script(manifest_dir, &harness_root, "typescript_probe.js");

  // Provide a TypeScript installation via TYPECHECK_TS_HARNESS_TYPESCRIPT_DIR.
  let ts_install = tmp.path().join("ts_install");
  write_stub_typescript(
    &ts_install.join("node_modules").join("typescript"),
    "99.99.99-test",
  );

  let probe = harness_root.join("scripts").join("typescript_probe.js");
  let output = node_command(&node_path)
    .arg(&probe)
    .env("TYPECHECK_TS_HARNESS_TYPESCRIPT_DIR", &ts_install)
    .output()
    .expect("run typescript_probe.js");

  assert!(
    output.status.success(),
    "expected probe success; stdout: {}\nstderr: {}",
    String::from_utf8_lossy(&output.stdout),
    String::from_utf8_lossy(&output.stderr)
  );

  let stdout = String::from_utf8_lossy(&output.stdout);
  assert_eq!(stdout.trim(), "99.99.99-test");
}
