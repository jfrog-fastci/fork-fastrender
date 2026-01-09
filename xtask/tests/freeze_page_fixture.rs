use fastrender::resource::{DEFAULT_ACCEPT_LANGUAGE, DEFAULT_USER_AGENT};
use std::process::Command;
use tempfile::TempDir;
use xtask::freeze_page_fixture::{plan_freeze_page_fixture, FreezePageFixturePlanArgs};

fn plan_for_page(page: &str) -> FreezePageFixturePlanArgs {
  let temp = TempDir::new().expect("tempdir");
  FreezePageFixturePlanArgs {
    pages: vec![page.to_string()],
    html_dir: temp.path().join("html"),
    asset_cache_dir: temp.path().join("assets"),
    fixtures_root: temp.path().join("fixtures"),
    bundle_out_dir: temp.path().join("bundles"),
    overwrite: true,
    allow_missing_resources: false,
    include_scripts: false,
    user_agent: DEFAULT_USER_AGENT.to_string(),
    accept_language: DEFAULT_ACCEPT_LANGUAGE.to_string(),
    viewport: (1200, 800),
    dpr: 1.0,
  }
}

#[test]
fn normalizes_url_to_cache_stem_fixture_name() {
  let args = plan_for_page("https://www.example.com/");
  let plan = plan_freeze_page_fixture(&args).expect("plan");
  assert_eq!(plan.pages.len(), 1);
  assert_eq!(plan.pages[0].fixture_name, "example.com");
}

#[test]
fn planned_bundle_page_cache_command_includes_required_args() {
  let temp = TempDir::new().expect("tempdir");
  let args = FreezePageFixturePlanArgs {
    pages: vec!["https://www.example.com/".to_string()],
    html_dir: temp.path().join("html"),
    asset_cache_dir: temp.path().join("assets"),
    fixtures_root: temp.path().join("fixtures"),
    bundle_out_dir: temp.path().join("bundles"),
    overwrite: true,
    allow_missing_resources: true,
    include_scripts: false,
    user_agent: DEFAULT_USER_AGENT.to_string(),
    accept_language: DEFAULT_ACCEPT_LANGUAGE.to_string(),
    viewport: (1200, 800),
    dpr: 1.0,
  };
  let plan = plan_freeze_page_fixture(&args).expect("plan");
  let cmd = &plan.pages[0].bundle_command;
  let joined = cmd.args.join(" ");

  assert!(
    joined.contains("--asset-cache-dir"),
    "expected bundle_page cache command to include --asset-cache-dir, got: {joined}"
  );
  assert!(
    joined.contains("--user-agent") && joined.contains(DEFAULT_USER_AGENT),
    "expected command to include --user-agent, got: {joined}"
  );
  assert!(
    joined.contains("--accept-language") && joined.contains(DEFAULT_ACCEPT_LANGUAGE),
    "expected command to include --accept-language, got: {joined}"
  );
  assert!(
    joined.contains("--viewport") && joined.contains("1200x800"),
    "expected command to include --viewport, got: {joined}"
  );
  assert!(
    joined.contains("--dpr"),
    "expected command to include --dpr, got: {joined}"
  );
  assert!(
    joined.contains("--allow-missing"),
    "expected allow-missing-resources to map to --allow-missing, got: {joined}"
  );
}

#[test]
fn cli_errors_when_no_pages_specified() {
  let output = Command::new(env!("CARGO_BIN_EXE_xtask"))
    .args(["freeze-page-fixture"])
    .output()
    .expect("run xtask freeze-page-fixture");

  assert!(
    !output.status.success(),
    "expected freeze-page-fixture to fail when no pages are provided"
  );

  let stderr = String::from_utf8_lossy(&output.stderr);
  let stdout = String::from_utf8_lossy(&output.stdout);
  let combined = format!("{stdout}\n{stderr}");
  assert!(
    combined.to_ascii_lowercase().contains("no pages specified"),
    "expected error message to mention missing pages; got:\n{combined}"
  );
}
