use serde_json::json;
use sha2::{Digest, Sha256};
use std::fs;
use std::path::PathBuf;
use std::process::Command;
use tempfile::tempdir;

fn repo_root() -> PathBuf {
  let crate_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
  crate_dir
    .parent()
    .expect("xtask crate should live under the workspace root")
    .to_path_buf()
}

#[test]
fn importer_sniffs_woff2_magic_and_preserves_bytes_when_manifest_labels_as_html() {
  let repo_root = repo_root();

  let bundle_dir = tempdir().expect("bundle dir");
  let resources_dir = bundle_dir.path().join("resources");
  fs::create_dir_all(&resources_dir).expect("create resources dir");

  // WOFF2 signature (wOF2) followed by invalid UTF-8 so a lossy HTML rewrite would corrupt it.
  let font_bytes = vec![b'w', b'O', b'F', b'2', 0xff, 0xfe, 0xfd, 0x00, 0x01];
  fs::write(resources_dir.join("00000_font.html"), &font_bytes).expect("write font bytes");

  fs::write(
    bundle_dir.path().join("document.html"),
    "<!doctype html><html><body>ok</body></html>",
  )
  .expect("write document");

  let font_url = "https://fonts.example.test/font.html";
  let manifest = json!({
    "version": 1,
    "original_url": "https://example.test/",
    "document": {
      "path": "document.html",
      "content_type": "text/html; charset=utf-8",
      "final_url": "https://example.test/",
      "status": 200,
      "etag": null,
      "last_modified": null
    },
    "render": {
      "viewport": [800, 600],
      "device_pixel_ratio": 1.0,
      "scroll_x": 0.0,
      "scroll_y": 0.0,
      "full_page": false,
      "same_origin_subresources": false,
      "allowed_subresource_origins": []
    },
    "resources": {
      font_url: {
        "path": "resources/00000_font.html",
        // Mislabel the font as HTML (seen on real-world CDNs like `fonts.gstatic.com/l/font?...`).
        "content_type": "text/html; charset=utf-8",
        "status": 200,
        "final_url": font_url,
        "etag": null,
        "last_modified": null
      }
    }
  });
  fs::write(
    bundle_dir.path().join("bundle.json"),
    serde_json::to_vec_pretty(&manifest).expect("serialize manifest"),
  )
  .expect("write bundle.json");

  let output = tempdir().expect("output dir");
  let output_root = output.path().join("fixtures");
  let fixture_name = "font_sniff_fixture";

  let status = Command::new(env!("CARGO_BIN_EXE_xtask"))
    .current_dir(&repo_root)
    .arg("import-page-fixture")
    .arg(bundle_dir.path())
    .arg(fixture_name)
    .arg("--output-root")
    .arg(&output_root)
    .arg("--overwrite")
    .status()
    .expect("run importer");
  assert!(status.success(), "importer exited with {}", status);

  let digest = Sha256::digest(&font_bytes);
  let hash = digest
    .iter()
    .take(16)
    .map(|b| format!("{b:02x}"))
    .collect::<String>();
  let filename = format!("{hash}.woff2");

  let asset_path = output_root
    .join(fixture_name)
    .join("assets")
    .join(&filename);
  assert!(
    asset_path.is_file(),
    "expected importer to sniff wOF2 bytes and write {filename}, but it was missing"
  );

  let output_bytes = fs::read(&asset_path).expect("read output font bytes");
  assert_eq!(
    output_bytes, font_bytes,
    "expected importer to preserve font bytes (no lossy HTML rewrite)"
  );
}

