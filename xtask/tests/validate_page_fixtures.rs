use std::fs;
use std::process::Command;

use tempfile::tempdir;

fn run_validate(fixtures_root: &std::path::Path) -> std::process::Output {
  Command::new(env!("CARGO_BIN_EXE_xtask"))
    .args([
      "validate-page-fixtures",
      "--fixtures-root",
      fixtures_root.to_str().expect("fixtures root path"),
    ])
    .output()
    .expect("run cargo xtask validate-page-fixtures")
}

fn combined_output(output: &std::process::Output) -> String {
  let mut text = String::new();
  text.push_str(&String::from_utf8_lossy(&output.stdout));
  text.push_str(&String::from_utf8_lossy(&output.stderr));
  text
}

#[test]
fn validate_page_fixtures_fails_on_remote_img_in_index_html() {
  let dir = tempdir().expect("tempdir");
  let fixtures_root = dir.path();
  let fixture_dir = fixtures_root.join("example");
  fs::create_dir_all(&fixture_dir).expect("create fixture dir");

  fs::write(
    fixture_dir.join("index.html"),
    r#"<!doctype html><img src="https://example.com/x.png">"#,
  )
  .expect("write index.html");

  let output = run_validate(fixtures_root);
  assert!(
    !output.status.success(),
    "expected validator to fail, got: {:?}",
    output.status
  );
  let text = combined_output(&output);
  assert!(
    text.contains("example/index.html") && text.contains("https://example.com/x.png"),
    "expected output to mention failing file + URL; got:\n{text}"
  );
}

#[test]
fn validate_page_fixtures_fails_on_remote_img_in_embedded_html_asset() {
  let dir = tempdir().expect("tempdir");
  let fixtures_root = dir.path();
  let fixture_dir = fixtures_root.join("example");
  let assets_dir = fixture_dir.join("assets");
  fs::create_dir_all(&assets_dir).expect("create fixture dir");

  fs::write(
    fixture_dir.join("index.html"),
    r#"<!doctype html><iframe src="assets/frame.html"></iframe>"#,
  )
  .expect("write index.html");
  fs::write(
    assets_dir.join("frame.html"),
    r#"<!doctype html><img src="https://example.com/frame.png">"#,
  )
  .expect("write frame.html");

  let output = run_validate(fixtures_root);
  assert!(
    !output.status.success(),
    "expected validator to fail, got: {:?}",
    output.status
  );
  let text = combined_output(&output);
  assert!(
    text.contains("example/assets/frame.html") && text.contains("https://example.com/frame.png"),
    "expected output to mention embedded HTML asset + URL; got:\n{text}"
  );
}

#[test]
fn validate_page_fixtures_passes_for_clean_fixture() {
  let dir = tempdir().expect("tempdir");
  let fixtures_root = dir.path();
  let fixture_dir = fixtures_root.join("example");
  let assets_dir = fixture_dir.join("assets");
  fs::create_dir_all(&assets_dir).expect("create fixture dir");

  fs::write(
    fixture_dir.join("index.html"),
    r#"<!doctype html><img src="assets/local.png">"#,
  )
  .expect("write index.html");
  fs::write(assets_dir.join("local.png"), b"").expect("write local.png");

  let output = run_validate(fixtures_root);
  assert!(
    output.status.success(),
    "expected validator to succeed, got: {:?}\n{}",
    output.status,
    combined_output(&output)
  );
}

#[test]
fn validate_page_fixtures_ignores_css_namespace_urls() {
  let dir = tempdir().expect("tempdir");
  let fixtures_root = dir.path();
  let fixture_dir = fixtures_root.join("example");
  fs::create_dir_all(&fixture_dir).expect("create fixture dir");

  fs::write(fixture_dir.join("index.html"), r#"<!doctype html>"#).expect("write index.html");
  fs::write(
    fixture_dir.join("namespace.css"),
    r#"@namespace svg url("http://www.w3.org/2000/svg");"#,
  )
  .expect("write css");

  let output = run_validate(fixtures_root);
  assert!(
    output.status.success(),
    "expected validator to succeed, got: {:?}\n{}",
    output.status,
    combined_output(&output)
  );
}

#[test]
fn validate_repo_pages_regression_fixtures_are_offline() {
  let repo_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
    .parent()
    .expect("xtask should live at repo_root/xtask");
  let fixtures_root = repo_root.join("tests/pages/fixtures");
  let regression_manifest = [
    // Legacy location.
    repo_root.join("tests/pages_regression_test.rs"),
    // Current location (pages_regression suite lives under tests/regression/).
    repo_root.join("tests/regression/pages.rs"),
  ]
  .into_iter()
  .find(|path| path.is_file())
  .expect("expected a pages_regression manifest under tests/pages_regression_test.rs or tests/regression/pages.rs");

  let contents = fs::read_to_string(&regression_manifest)
    .unwrap_or_else(|e| panic!("read {}: {}", regression_manifest.display(), e));
  let mut fixtures = Vec::new();
  let mut seen = std::collections::BTreeSet::new();
  for line in contents.lines() {
    let line = line.trim();
    let Some(rest) = line.strip_prefix("name: \"") else {
      continue;
    };
    let Some(end) = rest.find('"') else {
      continue;
    };
    let name = &rest[..end];
    if seen.insert(name.to_string()) {
      fixtures.push(name.to_string());
    }
  }
  assert!(
    !fixtures.is_empty(),
    "expected to discover regression fixtures from {}",
    regression_manifest.display()
  );

  let output = Command::new(env!("CARGO_BIN_EXE_xtask"))
    .args([
      "validate-page-fixtures",
      "--fixtures-root",
      fixtures_root.to_str().expect("fixtures root path"),
      "--only",
      &fixtures.join(","),
    ])
    .output()
    .expect("run cargo xtask validate-page-fixtures (regression fixtures)");

  assert!(
    output.status.success(),
    "expected repo regression fixtures to be offline, got: {:?}\n{}",
    output.status,
    combined_output(&output)
  );
}
