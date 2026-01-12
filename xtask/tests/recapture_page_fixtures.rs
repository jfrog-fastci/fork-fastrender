use std::fs;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::Command;
use tempfile::tempdir;
use url::Url;

#[test]
fn recaptures_and_imports_file_fixture() {
  if cfg!(windows) {
    eprintln!("Skipping recapture-page-fixtures integration test on Windows.");
    return;
  }

  let crate_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
  let repo_root = crate_dir
    .parent()
    .expect("xtask crate should live under the workspace root");

  let temp = tempdir().expect("temp dir");
  let page_dir = temp.path().join("page");
  fs::create_dir_all(&page_dir).expect("create page dir");

  // Avoid spawning a nested `cargo run --bin bundle_page` (slow + contention-prone inside `cargo
  // test`) by providing a tiny stub `bundle_page` implementation.
  let bundle_page_stub = temp.path().join("bundle_page_stub.sh");
  fs::write(
    &bundle_page_stub,
    r#"#!/usr/bin/env bash
set -euo pipefail

subcmd="${1:-}"
if [[ -z "${subcmd}" ]]; then
  echo "missing subcommand" >&2
  exit 2
fi
shift
if [[ "${subcmd}" != "fetch" ]]; then
  echo "unsupported subcommand: ${subcmd}" >&2
  exit 2
fi

url="${1:-}"
if [[ -z "${url}" ]]; then
  echo "missing URL" >&2
  exit 2
fi
shift

out=""
while [[ $# -gt 0 ]]; do
  case "$1" in
    --out)
      out="$2"
      shift 2
      ;;
    *)
      shift 1
      ;;
  esac
done

if [[ -z "${out}" ]]; then
  echo "missing --out" >&2
  exit 2
fi

if [[ "${url}" != file://* ]]; then
  echo "expected file:// url, got ${url}" >&2
  exit 2
fi

doc_path="${url#file://}"
doc_dir="$(dirname "${doc_path}")"

css_path="${doc_dir}/styles.css"
img_path="${doc_dir}/image.png"

css_url="file://${css_path}"
img_url="file://${img_path}"

tmp_dir="$(mktemp -d)"
trap 'rm -rf "${tmp_dir}"' EXIT
mkdir -p "${tmp_dir}/resources"

cp "${doc_path}" "${tmp_dir}/document.html"
cp "${css_path}" "${tmp_dir}/resources/00000_styles.css"
cp "${img_path}" "${tmp_dir}/resources/00001_image.png"

cat > "${tmp_dir}/bundle.json" <<EOF
{
  "version": 1,
  "original_url": "${url}",
  "document": {
    "path": "document.html",
    "content_type": "text/html; charset=utf-8",
    "final_url": "${url}",
    "status": 200,
    "etag": null,
    "last_modified": null
  },
  "render": {
    "viewport": [1200, 800],
    "device_pixel_ratio": 1.0,
    "scroll_x": 0.0,
    "scroll_y": 0.0,
    "full_page": false,
    "same_origin_subresources": false,
    "allowed_subresource_origins": []
  },
  "resources": {
    "${css_url}": {
      "path": "resources/00000_styles.css",
      "content_type": "text/css; charset=utf-8",
      "status": 200,
      "final_url": "${css_url}",
      "etag": null,
      "last_modified": null
    },
    "${img_url}": {
      "path": "resources/00001_image.png",
      "content_type": "image/png",
      "status": 200,
      "final_url": "${img_url}",
      "etag": null,
      "last_modified": null
    }
  }
}
EOF

mkdir -p "$(dirname "${out}")"
tar -cf "${out}" -C "${tmp_dir}" bundle.json document.html resources
"#,
  )
  .expect("write bundle_page stub");
  #[cfg(unix)]
  {
    let mut perms = fs::metadata(&bundle_page_stub)
      .expect("stat bundle_page stub")
      .permissions();
    perms.set_mode(0o755);
    fs::set_permissions(&bundle_page_stub, perms).expect("chmod bundle_page stub");
  }

  // Simple HTML fixture with a linked stylesheet and an image so crawl mode captures subresources.
  fs::write(
    page_dir.join("index.html"),
    r#"<!doctype html>
<html>
  <head>
    <meta charset="utf-8">
    <link rel="stylesheet" href="styles.css">
  </head>
  <body>
    <img src="image.png" alt="fixture">
  </body>
</html>
"#,
  )
  .expect("write html");
  fs::write(
    page_dir.join("styles.css"),
    "body { background-image: url('image.png'); }",
  )
  .expect("write css");
  fs::write(page_dir.join("image.png"), b"not a real png").expect("write image");

  let file_url = Url::from_file_path(page_dir.join("index.html"))
    .expect("file:// url")
    .to_string();

  let manifest_path = temp.path().join("manifest.json");
  fs::write(
    &manifest_path,
    format!(
      r#"{{
  "fixtures": [
    {{
      "name": "local_file_fixture",
      "url": "{file_url}",
      "viewport": [1200, 800],
      "dpr": 1.0
    }}
  ]
}}"#
    ),
  )
  .expect("write manifest");

  let fixtures_root = temp.path().join("fixtures");
  let bundles_root = temp.path().join("bundles");

  let status = Command::new(env!("CARGO_BIN_EXE_xtask"))
    .current_dir(repo_root)
    .env("FASTR_XTASK_BUNDLE_PAGE_BIN", &bundle_page_stub)
    .arg("recapture-page-fixtures")
    .arg("--manifest")
    .arg(&manifest_path)
    .arg("--fixtures-root")
    .arg(&fixtures_root)
    .arg("--bundle-out-dir")
    .arg(&bundles_root)
    .arg("--debug")
    .status()
    .expect("run recapture-page-fixtures");
  assert!(status.success(), "command exited with {}", status);

  let fixture_dir = fixtures_root.join("local_file_fixture");
  let index = fixture_dir.join("index.html");
  assert!(index.is_file(), "fixture index.html should exist");

  let html = fs::read_to_string(&index).expect("read imported html");
  assert!(
    !html.contains("http://") && !html.contains("https://") && !html.contains("file://"),
    "imported fixture should not contain remote/file references; got:\n{html}"
  );
  assert!(
    fixture_dir.join("assets").is_dir(),
    "assets directory should exist"
  );
}
