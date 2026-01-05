use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use tempfile::tempdir;

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

fn write_fake_chrome(dir: &Path) -> PathBuf {
  let path = dir.join("fake_chrome");
  // Minimal Chrome/Chromium stub:
  // - `--version` prints a plausible version string.
  // - Any invocation that includes `--screenshot=<path>` writes a tiny PNG to that path.
  let script = r#"#!/usr/bin/env python3
import base64
import os
import sys

if "--version" in sys.argv:
    print("Chromium 123.0.0.0")
    sys.exit(0)

screenshot = None
for idx, arg in enumerate(sys.argv):
    if arg.startswith("--screenshot="):
        screenshot = arg.split("=", 1)[1]
        break
    if arg == "--screenshot" and idx + 1 < len(sys.argv):
        screenshot = sys.argv[idx + 1]
        break

if screenshot:
    os.makedirs(os.path.dirname(screenshot), exist_ok=True)
    png = base64.b64decode(
        "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMB/6XnXQAAAABJRU5ErkJggg=="
    )
    with open(screenshot, "wb") as f:
        f.write(png)

sys.exit(0)
"#;
  fs::write(&path, script).expect("write fake chrome");

  #[cfg(unix)]
  {
    let mut perms = fs::metadata(&path).expect("stat fake chrome").permissions();
    perms.set_mode(0o755);
    fs::set_permissions(&path, perms).expect("chmod fake chrome");
  }

  path
}

#[test]
#[cfg(unix)]
fn chrome_baseline_script_parses_flags_without_treating_them_as_page_stems() {
  if Command::new("python3").arg("--version").output().is_err() {
    eprintln!("skipping chrome_baseline_script_parses_flags_without_treating_them_as_page_stems: python3 not available");
    return;
  }

  let tmp = tempdir().expect("tempdir");
  let html_dir = tmp.path().join("html");
  let out_dir = tmp.path().join("out");
  let bin_dir = tmp.path().join("bin");
  fs::create_dir_all(&html_dir).expect("create html dir");
  fs::create_dir_all(&out_dir).expect("create out dir");
  fs::create_dir_all(&bin_dir).expect("create bin dir");

  // Create a minimal cached HTML snapshot (+ sidecar) to satisfy the script.
  fs::write(
    html_dir.join("page.html"),
    r#"<!doctype html>
<html>
  <head><meta charset="utf-8"></head>
  <body>Hello</body>
</html>
"#,
  )
  .expect("write cached html");
  fs::write(html_dir.join("page.html.meta"), "url: https://example.com/\n").expect("write meta");

  let fake_chrome = write_fake_chrome(&bin_dir);
  let repo_root = Path::new(env!("CARGO_MANIFEST_DIR"));
  let script_path = repo_root.join("scripts/chrome_baseline.sh");

  // Use a non-default DPR/viewport so the metadata proves the flags were actually applied.
  let output = Command::new(&script_path)
    .env("CHROME_BIN", &fake_chrome)
    .args([
      "--html-dir",
      html_dir.to_str().unwrap(),
      "--out-dir",
      out_dir.to_str().unwrap(),
      "--viewport",
      "200x150",
      "--dpr",
      "1.25",
      "--timeout",
      "5",
      "--",
      "page",
    ])
    .output()
    .expect("run scripts/chrome_baseline.sh");

  assert!(
    output.status.success(),
    "expected scripts/chrome_baseline.sh to succeed, got status={:?}\nstdout:\n{}\nstderr:\n{}",
    output.status.code(),
    String::from_utf8_lossy(&output.stdout),
    String::from_utf8_lossy(&output.stderr),
  );

  let png_path = out_dir.join("page.png");
  assert!(png_path.is_file(), "expected {} to exist", png_path.display());
  assert!(
    fs::metadata(&png_path).expect("stat png").len() > 0,
    "expected {} to be non-empty",
    png_path.display()
  );

  let meta: Value =
    serde_json::from_slice(&fs::read(out_dir.join("page.json")).expect("read json"))
      .expect("parse json");
  assert_eq!(meta["stem"], "page");
  assert_eq!(meta["viewport"], serde_json::json!([200, 150]));
  assert_eq!(meta["dpr"], serde_json::json!(1.25));
  assert_eq!(meta["js"], "off");
  assert_eq!(meta["headless"], "new");
  assert!(
    meta["html_sha256"].as_str().is_some_and(|s| s.len() == 64),
    "expected html_sha256 to be a 64-char hex string, got: {}",
    meta["html_sha256"]
  );
}

#[test]
#[cfg(unix)]
fn chrome_baseline_script_errors_on_unknown_flag() {
  let repo_root = Path::new(env!("CARGO_MANIFEST_DIR"));
  let script_path = repo_root.join("scripts/chrome_baseline.sh");

  let output = Command::new(&script_path)
    .arg("--definitely-not-a-flag")
    .output()
    .expect("run scripts/chrome_baseline.sh");

  assert!(
    !output.status.success(),
    "expected non-zero exit for unknown flag, got {:?}",
    output.status.code()
  );
  let stderr = String::from_utf8_lossy(&output.stderr);
  assert!(
    stderr.contains("unknown option"),
    "expected stderr to mention unknown option, got:\n{stderr}"
  );
}
