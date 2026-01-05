use image::GenericImageView;
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
  // - Any invocation that includes `--screenshot=<path>` writes a deterministic PNG sized according
  //   to `--window-size` and `--force-device-scale-factor`.
  let script = r#"#!/usr/bin/env python3
import math
import os
import struct
import sys
import zlib

if "--version" in sys.argv:
    print("Chromium 123.0.0.0")
    sys.exit(0)

print("stub chrome args:", " ".join(sys.argv[1:]))

screenshot = None
window_w = 1
window_h = 1
dpr = 1.0
for idx, arg in enumerate(sys.argv):
    if arg.startswith("--screenshot="):
        screenshot = arg.split("=", 1)[1]
        break
    if arg == "--screenshot" and idx + 1 < len(sys.argv):
        screenshot = sys.argv[idx + 1]
        break
    if arg.startswith("--window-size="):
        size = arg.split("=", 1)[1]
        if "," in size:
            try:
                parts = size.split(",", 1)
                window_w = max(1, int(parts[0]))
                window_h = max(1, int(parts[1]))
            except Exception:
                pass
    if arg.startswith("--force-device-scale-factor="):
        try:
            dpr = float(arg.split("=", 1)[1])
        except Exception:
            pass

def round_half_up(x: float) -> int:
    return int(math.floor(x + 0.5))

if screenshot:
    os.makedirs(os.path.dirname(screenshot), exist_ok=True)
    w = max(1, round_half_up(window_w * dpr))
    h = max(1, round_half_up(window_h * dpr))

    def chunk(typ: bytes, payload: bytes) -> bytes:
        crc = zlib.crc32(typ)
        crc = zlib.crc32(payload, crc) & 0xFFFFFFFF
        return struct.pack(">I", len(payload)) + typ + payload + struct.pack(">I", crc)

    # Solid green RGBA with filter type 0 per scanline.
    row = b"\x00" + (b"\x00\xff\x00\xff" * w)
    raw = row * h
    compressed = zlib.compress(raw)
    ihdr = struct.pack(">IIBBBBB", w, h, 8, 6, 0, 0, 0)
    png = (
        b"\x89PNG\r\n\x1a\n"
        + chunk(b"IHDR", ihdr)
        + chunk(b"IDAT", compressed)
        + chunk(b"IEND", b"")
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
  fs::write(
    html_dir.join("page.html.meta"),
    "url: https://example.com/\n",
  )
  .expect("write meta");

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
  assert!(
    png_path.is_file(),
    "expected {} to exist",
    png_path.display()
  );
  assert!(
    fs::metadata(&png_path).expect("stat png").len() > 0,
    "expected {} to be non-empty",
    png_path.display()
  );

  let img = image::open(&png_path).expect("decode output PNG");
  assert_eq!(img.dimensions(), (250, 188));

  let chrome_log = fs::read_to_string(out_dir.join("page.chrome.log")).expect("read chrome log");
  assert!(
    chrome_log.contains("--window-size=200,238"),
    "expected headless baseline to pad window size then crop, got log:\n{chrome_log}"
  );
  assert!(
    chrome_log.contains("--force-device-scale-factor=1.25"),
    "expected headless baseline to apply the requested DPR, got log:\n{chrome_log}"
  );

  let meta: Value =
    serde_json::from_slice(&fs::read(out_dir.join("page.json")).expect("read json"))
      .expect("parse json");
  assert_eq!(meta["stem"], "page");
  assert_eq!(meta["viewport"], serde_json::json!([200, 150]));
  assert_eq!(meta["chrome_window"], serde_json::json!([200, 238]));
  assert_eq!(meta["chrome_window_padding_css"], serde_json::json!(88));
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
