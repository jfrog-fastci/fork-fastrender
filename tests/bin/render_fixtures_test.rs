use image::GenericImageView;
use std::fs;
use std::path::Path;
use std::process::Command;
use tempfile::TempDir;
use walkdir::WalkDir;

use sha2::{Digest, Sha256};

fn sha256_hex(bytes: &[u8]) -> String {
  let digest = Sha256::digest(bytes);
  digest.iter().map(|b| format!("{b:02x}")).collect()
}

fn normalize_rel_path(path: &Path) -> String {
  path
    .components()
    .map(|c| c.as_os_str().to_string_lossy())
    .collect::<Vec<_>>()
    .join("/")
}

fn hash_fixture_dir_sha256(dir: &Path) -> String {
  // Keep this hashing algorithm in sync with `render_fixtures` + `xtask fixture-chrome-diff`.
  let mut files = Vec::new();
  for entry in WalkDir::new(dir).follow_links(false) {
    let entry = entry.expect("walk fixture dir");
    if !entry.file_type().is_file() {
      continue;
    }
    let rel = entry
      .path()
      .strip_prefix(dir)
      .expect("strip fixture dir prefix");
    files.push((normalize_rel_path(rel), entry.path().to_path_buf()));
  }
  files.sort_by(|a, b| a.0.cmp(&b.0));

  let mut hasher = Sha256::new();
  for (rel, path) in files {
    hasher.update(rel.as_bytes());
    hasher.update([0u8]);
    hasher.update(fs::read(path).expect("read fixture file"));
  }
  hasher
    .finalize()
    .iter()
    .map(|b| format!("{b:02x}"))
    .collect()
}

fn write_fixture(root: &std::path::Path, stem: &str, index_html: &str) -> std::path::PathBuf {
  let dir = root.join(stem);
  fs::create_dir_all(&dir).expect("create fixture dir");
  fs::write(dir.join("index.html"), index_html).expect("write index.html");
  dir
}

#[test]
fn render_fixtures_writes_png_output() {
  let temp = TempDir::new().expect("tempdir");
  let fixtures_dir = temp.path().join("fixtures");
  let out_dir = temp.path().join("out");
  fs::create_dir_all(&fixtures_dir).expect("create fixtures dir");

  write_fixture(
    &fixtures_dir,
    "basic",
    "<!doctype html><html><body>ok</body></html>",
  );

  let status = Command::new(env!("CARGO_BIN_EXE_render_fixtures"))
    .current_dir(temp.path())
    // Keep the child process predictable and avoid spinning up a huge global Rayon pool on large
    // CI machines. `render_fixtures` uses its own harness pool for fixture-level concurrency.
    .env("RAYON_NUM_THREADS", "2")
    // Ensure the paint pipeline stays on the global pool for this harness-level test.
    .env("FASTR_PAINT_THREADS", "1")
    .args([
      "--fixtures-dir",
      fixtures_dir.to_str().unwrap(),
      "--out-dir",
      out_dir.to_str().unwrap(),
      "--fixtures",
      "basic",
      "--viewport",
      "64x64",
      "--jobs",
      "1",
      "--timeout",
      "2",
    ])
    .status()
    .expect("run render_fixtures");

  assert!(status.success(), "expected render_fixtures to succeed");
  assert!(out_dir.join("basic.png").is_file(), "expected PNG output");
  assert!(
    out_dir.join("basic.log").is_file(),
    "expected per-fixture log"
  );
  assert!(
    out_dir.join("basic.json").is_file(),
    "expected per-fixture metadata json"
  );

  let metadata_bytes = fs::read(out_dir.join("basic.json")).expect("read metadata");
  let metadata: serde_json::Value =
    serde_json::from_slice(&metadata_bytes).expect("parse metadata json");
  assert_eq!(metadata["fixture"], "basic");
  assert_eq!(metadata["viewport"], serde_json::json!([64, 64]));
  assert_eq!(metadata["media"], "screen");
  assert_eq!(metadata["fit_canvas_to_content"], false);
  assert_eq!(metadata["timeout_secs"], 2);
  assert_eq!(metadata["status"], "ok");
  assert!(
    metadata["dpr"].as_f64().unwrap_or_default() > 0.0,
    "expected dpr to be a positive number"
  );

  let input_sha256 = metadata["input_sha256"]
    .as_str()
    .expect("input_sha256 should be present");
  let expected_input_sha256 = sha256_hex(
    &fs::read(fixtures_dir.join("basic").join("index.html")).expect("read fixture html bytes"),
  );
  assert_eq!(
    input_sha256, expected_input_sha256,
    "input_sha256 should match fixture index.html contents"
  );

  let fixture_dir_sha256 = metadata["fixture_dir_sha256"]
    .as_str()
    .expect("fixture_dir_sha256 should be present");
  let expected_dir_sha256 = hash_fixture_dir_sha256(&fixtures_dir.join("basic"));
  assert_eq!(
    fixture_dir_sha256, expected_dir_sha256,
    "fixture_dir_sha256 should match fixture directory contents"
  );
}

#[test]
fn render_fixtures_help_mentions_determinism_flags() {
  let output = Command::new(env!("CARGO_BIN_EXE_render_fixtures"))
    .arg("--help")
    .output()
    .expect("run render_fixtures --help");

  assert!(
    output.status.success(),
    "render_fixtures --help should exit successfully"
  );

  // clap writes help to stdout; keep stderr for compatibility with older parsers
  let help = if output.stderr.is_empty() {
    String::from_utf8_lossy(&output.stdout)
  } else {
    String::from_utf8_lossy(&output.stderr)
  };

  assert!(
    help.contains("--repeat")
      && help.contains("--shuffle")
      && help.contains("--seed")
      && help.contains("--fail-on-nondeterminism")
      && help.contains("--save-variants")
      && help.contains("--reset-paint-scratch"),
    "help output should mention determinism flags; got:\n{help}"
  );
}

#[test]
fn render_fixtures_shuffle_requires_repeat_gt_one() {
  let temp = TempDir::new().expect("tempdir");
  let fixtures_dir = temp.path().join("fixtures");
  let out_dir = temp.path().join("out");
  fs::create_dir_all(&fixtures_dir).expect("create fixtures dir");

  write_fixture(
    &fixtures_dir,
    "basic",
    "<!doctype html><html><body>ok</body></html>",
  );

  let output = Command::new(env!("CARGO_BIN_EXE_render_fixtures"))
    .current_dir(temp.path())
    .env("RAYON_NUM_THREADS", "2")
    .env("FASTR_PAINT_THREADS", "1")
    .args([
      "--fixtures-dir",
      fixtures_dir.to_str().unwrap(),
      "--out-dir",
      out_dir.to_str().unwrap(),
      "--fixtures",
      "basic",
      "--viewport",
      "64x64",
      "--jobs",
      "1",
      "--timeout",
      "2",
      "--shuffle",
    ])
    .output()
    .expect("run render_fixtures");

  assert!(
    !output.status.success(),
    "expected render_fixtures to fail when --shuffle is used without --repeat > 1"
  );
  let stderr = String::from_utf8_lossy(&output.stderr);
  assert!(
    stderr.contains("shuffle requires --repeat > 1"),
    "expected error message to mention repeat requirement; got:\n{stderr}"
  );
}

#[test]
fn render_fixtures_save_variants_requires_repeat_gt_one() {
  let temp = TempDir::new().expect("tempdir");
  let fixtures_dir = temp.path().join("fixtures");
  let out_dir = temp.path().join("out");
  fs::create_dir_all(&fixtures_dir).expect("create fixtures dir");

  write_fixture(
    &fixtures_dir,
    "basic",
    "<!doctype html><html><body>ok</body></html>",
  );

  let output = Command::new(env!("CARGO_BIN_EXE_render_fixtures"))
    .current_dir(temp.path())
    .env("RAYON_NUM_THREADS", "2")
    .env("FASTR_PAINT_THREADS", "1")
    .args([
      "--fixtures-dir",
      fixtures_dir.to_str().unwrap(),
      "--out-dir",
      out_dir.to_str().unwrap(),
      "--fixtures",
      "basic",
      "--viewport",
      "64x64",
      "--jobs",
      "1",
      "--timeout",
      "2",
      "--save-variants",
    ])
    .output()
    .expect("run render_fixtures");

  assert!(
    !output.status.success(),
    "expected render_fixtures to fail when --save-variants is used without --repeat > 1"
  );
  let stderr = String::from_utf8_lossy(&output.stderr);
  assert!(
    stderr.contains("save-variants requires --repeat > 1"),
    "expected error message to mention repeat requirement; got:\n{stderr}"
  );
}

#[test]
fn render_fixtures_fail_on_nondeterminism_requires_repeat_gt_one() {
  let temp = TempDir::new().expect("tempdir");
  let fixtures_dir = temp.path().join("fixtures");
  let out_dir = temp.path().join("out");
  fs::create_dir_all(&fixtures_dir).expect("create fixtures dir");

  write_fixture(
    &fixtures_dir,
    "basic",
    "<!doctype html><html><body>ok</body></html>",
  );

  let output = Command::new(env!("CARGO_BIN_EXE_render_fixtures"))
    .current_dir(temp.path())
    .env("RAYON_NUM_THREADS", "2")
    .env("FASTR_PAINT_THREADS", "1")
    .args([
      "--fixtures-dir",
      fixtures_dir.to_str().unwrap(),
      "--out-dir",
      out_dir.to_str().unwrap(),
      "--fixtures",
      "basic",
      "--viewport",
      "64x64",
      "--jobs",
      "1",
      "--timeout",
      "2",
      "--fail-on-nondeterminism",
    ])
    .output()
    .expect("run render_fixtures");

  assert!(
    !output.status.success(),
    "expected render_fixtures to fail when --fail-on-nondeterminism is used without --repeat > 1"
  );
  let stderr = String::from_utf8_lossy(&output.stderr);
  assert!(
    stderr.contains("fail-on-nondeterminism requires --repeat > 1"),
    "expected error message to mention repeat requirement; got:\n{stderr}"
  );
}

#[test]
fn render_fixtures_repeat_mode_is_deterministic() {
  let temp = TempDir::new().expect("tempdir");
  let fixtures_dir = temp.path().join("fixtures");
  let out_dir = temp.path().join("out");
  fs::create_dir_all(&fixtures_dir).expect("create fixtures dir");

  write_fixture(
    &fixtures_dir,
    "repeat",
    "<!doctype html><html><body><div style=\"width:32px;height:32px;background:#f0f\"></div></body></html>",
  );

  let status = Command::new(env!("CARGO_BIN_EXE_render_fixtures"))
    .current_dir(temp.path())
    .env("RAYON_NUM_THREADS", "2")
    .env("FASTR_PAINT_THREADS", "1")
    .args([
      "--fixtures-dir",
      fixtures_dir.to_str().unwrap(),
      "--out-dir",
      out_dir.to_str().unwrap(),
      "--fixtures",
      "repeat",
      "--viewport",
      "64x64",
      "--jobs",
      "2",
      "--timeout",
      "5",
      "--repeat",
      "2",
      "--shuffle",
      "--seed",
      "1",
      "--fail-on-nondeterminism",
      "--save-variants",
      "--reset-paint-scratch",
    ])
    .status()
    .expect("run render_fixtures");

  assert!(
    status.success(),
    "expected render_fixtures repeat mode to succeed for a deterministic fixture"
  );
  assert!(
    out_dir.join("repeat.png").is_file(),
    "expected baseline PNG output"
  );
  assert!(
    !out_dir.join("repeat").join("nondeterminism").exists(),
    "did not expect nondeterminism outputs for a deterministic fixture"
  );
}

#[test]
fn render_fixtures_blocks_http_subresources() {
  let temp = TempDir::new().expect("tempdir");
  let fixtures_dir = temp.path().join("fixtures");
  let out_dir = temp.path().join("out");
  fs::create_dir_all(&fixtures_dir).expect("create fixtures dir");

  write_fixture(
    &fixtures_dir,
    "blocked",
    "<!doctype html><html><body><img src=\"http://example.com/a.png\"></body></html>",
  );

  let status = Command::new(env!("CARGO_BIN_EXE_render_fixtures"))
    .current_dir(temp.path())
    .env("RAYON_NUM_THREADS", "2")
    .env("FASTR_PAINT_THREADS", "1")
    .args([
      "--fixtures-dir",
      fixtures_dir.to_str().unwrap(),
      "--out-dir",
      out_dir.to_str().unwrap(),
      "--fixtures",
      "blocked",
      "--viewport",
      "64x64",
      "--jobs",
      "1",
      "--timeout",
      "2",
    ])
    .status()
    .expect("run render_fixtures");

  assert!(
    !status.success(),
    "expected render_fixtures to fail when http subresources are referenced"
  );

  let log = fs::read_to_string(out_dir.join("blocked.log")).expect("read log");
  assert!(
    log.contains("http://example.com/a.png"),
    "log should mention blocked URL"
  );
}

#[test]
fn render_fixtures_resolves_relative_stylesheets_from_base_url() {
  let temp = TempDir::new().expect("tempdir");
  let fixtures_dir = temp.path().join("fixtures");
  let out_dir = temp.path().join("out");
  fs::create_dir_all(&fixtures_dir).expect("create fixtures dir");

  let fixture_dir = write_fixture(
    &fixtures_dir,
    "relative_css",
    r#"<!doctype html>
<html>
  <head>
    <link rel="stylesheet" href="support/style.css">
  </head>
  <body></body>
</html>"#,
  );
  fs::create_dir_all(fixture_dir.join("support")).expect("create support dir");
  fs::write(
    fixture_dir.join("support/style.css"),
    "html, body { margin: 0; width: 100%; height: 100%; background: rgb(255, 0, 0); }",
  )
  .expect("write style.css");

  let status = Command::new(env!("CARGO_BIN_EXE_render_fixtures"))
    .current_dir(temp.path())
    .env("RAYON_NUM_THREADS", "2")
    .env("FASTR_PAINT_THREADS", "1")
    .args([
      "--fixtures-dir",
      fixtures_dir.to_str().unwrap(),
      "--out-dir",
      out_dir.to_str().unwrap(),
      "--fixtures",
      "relative_css",
      "--viewport",
      "32x32",
      "--jobs",
      "1",
      "--timeout",
      "2",
    ])
    .status()
    .expect("run render_fixtures");

  assert!(
    status.success(),
    "expected render_fixtures to succeed with relative stylesheet"
  );

  let png_bytes = fs::read(out_dir.join("relative_css.png")).expect("read png");
  let image = image::load_from_memory(&png_bytes)
    .expect("decode png")
    .to_rgba8();
  let pixel = image.get_pixel(0, 0).0;
  assert!(
    pixel[0] > 200 && pixel[1] < 50 && pixel[2] < 50,
    "expected red-ish background pixel from stylesheet (got {:?})",
    pixel
  );
}

#[test]
fn render_fixtures_writes_snapshot_outputs() {
  let temp = TempDir::new().expect("tempdir");
  let fixtures_dir = temp.path().join("fixtures");
  let out_dir = temp.path().join("out");
  fs::create_dir_all(&fixtures_dir).expect("create fixtures dir");

  write_fixture(
    &fixtures_dir,
    "snapshot",
    "<!doctype html><html><body style=\"margin:0;background:rgb(0,255,0)\"></body></html>",
  );

  let status = Command::new(env!("CARGO_BIN_EXE_render_fixtures"))
    .current_dir(temp.path())
    .env("RAYON_NUM_THREADS", "2")
    .env("FASTR_PAINT_THREADS", "1")
    .args([
      "--fixtures-dir",
      fixtures_dir.to_str().unwrap(),
      "--out-dir",
      out_dir.to_str().unwrap(),
      "--fixtures",
      "snapshot",
      "--viewport",
      "32x32",
      "--jobs",
      "1",
      "--timeout",
      "2",
      "--write-snapshot",
    ])
    .status()
    .expect("run render_fixtures");

  assert!(
    status.success(),
    "expected render_fixtures to succeed with --write-snapshot"
  );

  assert!(
    out_dir.join("snapshot/snapshot.json").is_file(),
    "expected snapshot.json output"
  );
  assert!(
    out_dir.join("snapshot/diagnostics.json").is_file(),
    "expected diagnostics.json output"
  );
}
