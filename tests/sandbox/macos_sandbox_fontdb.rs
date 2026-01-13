use std::process::Command;

use fastrender::sandbox::macos::{
  apply_renderer_sandbox, MacosSandboxMode, MacosSandboxStatus,
};

#[test]
fn relaxed_sandbox_allows_fontdb_system_font_discovery() {
  const CHILD_ENV: &str = "FASTR_TEST_MACOS_RELAXED_SANDBOX_FONTDB_CHILD";
  let test_name = crate::common::libtest::exact_test_name(
    module_path!(),
    stringify!(relaxed_sandbox_allows_fontdb_system_font_discovery),
  );
  let is_child = std::env::var_os(CHILD_ENV).is_some();
  if is_child {
    let status = apply_renderer_sandbox(MacosSandboxMode::RendererSystemFonts)
      .expect("apply relaxed macOS renderer sandbox profile");
    assert!(
      matches!(
        status,
        MacosSandboxStatus::Applied | MacosSandboxStatus::AlreadySandboxed
      ),
      "unexpected macOS sandbox status when applying relaxed profile: {status:?}"
    );
    if matches!(status, MacosSandboxStatus::AlreadySandboxed) {
      eprintln!(
        "skipping fontdb sandbox test: process was already sandboxed (status={status:?})"
      );
      return;
    }

    let mut db = fontdb::Database::new();
    db.load_system_fonts();

    let face_count = db.faces().len();
    assert!(
      face_count > 0,
      "expected system font discovery to find at least one face under relaxed sandbox"
    );

    // Bonus sanity check: `fontdb` generic families (e.g. `sans-serif`) should still resolve.
    let query = fontdb::Query {
      families: &[fontdb::Family::SansSerif],
      weight: fontdb::Weight(400),
      stretch: fontdb::Stretch::Normal,
      style: fontdb::Style::Normal,
    };
    assert!(
      db.query(&query).is_some(),
      "expected fontdb generic sans-serif query to resolve under relaxed sandbox"
    );
    return;
  }

  // `sandbox_init` is irreversible. Run the actual sandboxed probe in a subprocess so it doesn't
  // affect the rest of the test suite.
  let exe = std::env::current_exe().expect("current test exe path");
  let output = Command::new(exe)
    .env(CHILD_ENV, "1")
    .arg("--exact")
    .arg(&test_name)
    .arg("--nocapture")
    .output()
    .expect("spawn child test process");
  assert!(
    output.status.success(),
    "child process should exit successfully (stdout={}, stderr={})",
    String::from_utf8_lossy(&output.stdout),
    String::from_utf8_lossy(&output.stderr)
  );
}
