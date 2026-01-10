use std::process::Command;

#[test]
fn analyze_lazy_loading_reports_common_data_attrs() {
  let dir = tempfile::TempDir::new().expect("tempdir");
  let report_path = dir.path().join("lazy_loading.json");

  let output = Command::new(env!("CARGO_BIN_EXE_xtask"))
    .args([
      "analyze-lazy-loading",
      "--fixtures-root",
      "tests/pages/fixtures",
      "--fixture",
      "espn.com",
      "--fixture",
      "usatoday.com",
      "--json",
    ])
    .arg(&report_path)
    .output()
    .expect("run cargo xtask analyze-lazy-loading");

  assert!(
    output.status.success(),
    "xtask analyze-lazy-loading should exit successfully\nstdout:\n{}\nstderr:\n{}",
    String::from_utf8_lossy(&output.stdout),
    String::from_utf8_lossy(&output.stderr)
  );

  let stdout = String::from_utf8_lossy(&output.stdout);
  assert!(
    stdout.contains("data-default-src"),
    "stdout should mention data-default-src when analyzing espn.com; got:\n{stdout}"
  );
  assert!(
    stdout.contains("data-gl-src"),
    "stdout should mention data-gl-src when analyzing usatoday.com; got:\n{stdout}"
  );

  let raw = std::fs::read_to_string(&report_path).expect("read JSON report");
  let json: serde_json::Value = serde_json::from_str(&raw).expect("parse JSON report");

  let img_attrs = &json["elements"]["img"]["data_url_attrs"];
  assert!(
    img_attrs.get("data-default-src").is_some(),
    "JSON report should contain img.data_url_attrs[\"data-default-src\"]; got:\n{raw}"
  );
  assert!(
    img_attrs.get("data-gl-src").is_some(),
    "JSON report should contain img.data_url_attrs[\"data-gl-src\"]; got:\n{raw}"
  );
}

