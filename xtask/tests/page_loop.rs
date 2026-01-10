use std::fs;
use std::path::PathBuf;
use std::process::Command;

use tempfile::tempdir;

fn repo_root() -> PathBuf {
  PathBuf::from(env!("CARGO_MANIFEST_DIR"))
    .parent()
    .expect("xtask crate should live under the workspace root")
    .to_path_buf()
}

fn write_progress_file(dir: &std::path::Path, stem: &str, json: &str) {
  fs::create_dir_all(dir).expect("create progress dir");
  fs::write(dir.join(format!("{stem}.json")), json).expect("write progress json");
}

#[test]
fn dry_run_prints_expected_plan() {
  let output = Command::new(env!("CARGO_BIN_EXE_xtask"))
    .current_dir(repo_root())
    // Ensure binary paths resolve deterministically even if the outer environment sets
    // CARGO_TARGET_DIR.
    .env("CARGO_TARGET_DIR", "")
    .args(["page-loop", "--fixture", "example.com", "--dry-run"])
    .output()
    .expect("run xtask page-loop --dry-run");

  assert!(
    output.status.success(),
    "expected page-loop dry-run to succeed.\nstdout:\n{}\nstderr:\n{}",
    String::from_utf8_lossy(&output.stdout),
    String::from_utf8_lossy(&output.stderr)
  );

  let stdout = String::from_utf8_lossy(&output.stdout);
  assert!(
    stdout.contains("page-loop plan:"),
    "expected plan header; got:\n{stdout}"
  );
  assert!(
    stdout.contains("fixture: example.com"),
    "expected fixture name in plan; got:\n{stdout}"
  );
  assert!(
    stdout.contains("scripts/cargo_agent.sh build --release --bin render_fixtures"),
    "expected render_fixtures build command to be present; got:\n{stdout}"
  );
  let render_line = stdout
    .lines()
    .find(|line| line.contains("scripts/run_limited.sh") && line.contains("render_fixtures") && line.contains("--fixtures-dir"))
    .expect("render_fixtures command line should be printed");
  assert!(
    render_line.contains("target/release/render_fixtures"),
    "expected render_fixtures to use target/release; got:\n{render_line}"
  );
  assert!(
    !render_line.contains("FASTR_USE_BUNDLED_FONTS=1"),
    "page-loop should not force bundled-only fonts for render_fixtures; got:\n{render_line}"
  );
  assert!(
    stdout.contains("target/page_loop") && stdout.contains("example.com") && stdout.contains("fastrender"),
    "expected output path to mention target/page_loop/<fixture>/fastrender; got:\n{stdout}"
  );
}

#[test]
fn dry_run_with_debug_omits_release_for_fastrender_commands() {
  let output = Command::new(env!("CARGO_BIN_EXE_xtask"))
    .current_dir(repo_root())
    // Ensure diff_renders paths resolve deterministically even if the outer environment sets
    // CARGO_TARGET_DIR.
    .env("CARGO_TARGET_DIR", "")
    .args(["page-loop", "--fixture", "example.com", "--debug", "--dry-run"])
    .output()
    .expect("run xtask page-loop --debug --dry-run");

  assert!(
    output.status.success(),
    "expected page-loop --debug dry-run to succeed.\nstdout:\n{}\nstderr:\n{}",
    String::from_utf8_lossy(&output.stdout),
    String::from_utf8_lossy(&output.stderr)
  );

  let stdout = String::from_utf8_lossy(&output.stdout);
  let build_line = stdout
    .lines()
    .find(|line| line.contains("scripts/cargo_agent.sh build") && line.contains("--bin render_fixtures"))
    .expect("render_fixtures build command line should be printed");
  assert!(
    !build_line.contains("--release"),
    "expected --debug to omit `--release` for render_fixtures build; got:\n{build_line}"
  );

  let render_line = stdout
    .lines()
    .find(|line| line.contains("scripts/run_limited.sh") && line.contains("render_fixtures") && line.contains("--fixtures-dir"))
    .expect("render_fixtures execution command line should be printed");
  assert!(
    render_line.contains("target/debug/render_fixtures"),
    "expected --debug to use target/debug/render_fixtures; got:\n{render_line}"
  );
}

#[test]
fn dry_run_with_chrome_and_debug_uses_debug_diff_renders_binary() {
  let output = Command::new(env!("CARGO_BIN_EXE_xtask"))
    .current_dir(repo_root())
    .env("CARGO_TARGET_DIR", "")
    .args([
      "page-loop",
      "--fixture",
      "example.com",
      "--chrome",
      "--debug",
      "--dry-run",
    ])
    .output()
    .expect("run xtask page-loop --chrome --debug --dry-run");

  assert!(
    output.status.success(),
    "expected page-loop --chrome --debug dry-run to succeed.\nstdout:\n{}\nstderr:\n{}",
    String::from_utf8_lossy(&output.stdout),
    String::from_utf8_lossy(&output.stderr)
  );

  let stdout = String::from_utf8_lossy(&output.stdout);
  let build_line = stdout
    .lines()
    .find(|line| line.contains("scripts/cargo_agent.sh build") && line.contains("--bin diff_renders"))
    .expect("diff_renders build command line should be printed");
  assert!(
    !build_line.contains("--release"),
    "expected --debug to omit `--release` for diff_renders build; got:\n{build_line}"
  );

  let diff_line = stdout
    .lines()
    .find(|line| line.contains("scripts/run_limited.sh") && line.contains("diff_renders"))
    .expect("diff_renders execution command line should be printed");
  assert!(
    diff_line.contains("target/debug/diff_renders"),
    "expected --debug to use target/debug/diff_renders; got:\n{diff_line}"
  );
}

#[test]
fn dry_run_with_chrome_enables_chrome_patching_and_diff_steps() {
  let output = Command::new(env!("CARGO_BIN_EXE_xtask"))
    .current_dir(repo_root())
    .args(["page-loop", "--fixture", "example.com", "--chrome", "--dry-run"])
    .output()
    .expect("run xtask page-loop --chrome --dry-run");

  assert!(
    output.status.success(),
    "expected page-loop dry-run to succeed.\nstdout:\n{}\nstderr:\n{}",
    String::from_utf8_lossy(&output.stdout),
    String::from_utf8_lossy(&output.stderr)
  );

  let stdout = String::from_utf8_lossy(&output.stdout);
  assert!(
    stdout.contains("--patch-html-for-chrome-baseline"),
    "expected render_fixtures to be patched for chrome baselines; got:\n{stdout}"
  );
  assert!(
    stdout.contains("chrome-baseline-fixtures"),
    "expected chrome-baseline-fixtures command in plan; got:\n{stdout}"
  );
  let chrome_line = stdout
    .lines()
    .find(|line| line.contains("chrome-baseline-fixtures"))
    .expect("chrome-baseline-fixtures command line should be printed");
  assert!(
    chrome_line.contains("scripts/run_limited.sh"),
    "expected chrome-baseline-fixtures to run under scripts/run_limited.sh; got:\n{chrome_line}"
  );
  assert!(
    stdout.contains("--bin diff_renders") || stdout.contains("diff_renders"),
    "expected diff_renders commands in plan; got:\n{stdout}"
  );
}

#[test]
fn dry_run_forwards_timeout_to_all_steps() {
  let output = Command::new(env!("CARGO_BIN_EXE_xtask"))
    .current_dir(repo_root())
    .args([
      "page-loop",
      "--fixture",
      "example.com",
      "--chrome",
      "--timeout",
      "42",
      "--dry-run",
    ])
    .output()
    .expect("run xtask page-loop --timeout 42 --dry-run");

  assert!(
    output.status.success(),
    "expected page-loop dry-run to succeed.\nstdout:\n{}\nstderr:\n{}",
    String::from_utf8_lossy(&output.stdout),
    String::from_utf8_lossy(&output.stderr)
  );

  let stdout = String::from_utf8_lossy(&output.stdout);
  let occurrences = stdout.matches("--timeout 42").count();
  assert!(
    occurrences >= 2,
    "expected timeout to be forwarded to FastRender + Chrome commands; got:\n{stdout}"
  );
}

#[test]
fn dry_run_accepts_pageset_url() {
  let output = Command::new(env!("CARGO_BIN_EXE_xtask"))
    .current_dir(repo_root())
    .args(["page-loop", "--pageset", "https://example.com", "--dry-run"])
    .output()
    .expect("run xtask page-loop --pageset --dry-run");

  assert!(
    output.status.success(),
    "expected page-loop --pageset dry-run to succeed.\nstdout:\n{}\nstderr:\n{}",
    String::from_utf8_lossy(&output.stdout),
    String::from_utf8_lossy(&output.stderr)
  );

  let stdout = String::from_utf8_lossy(&output.stdout);
  assert!(
    stdout.contains("fixture: example.com"),
    "expected pageset URL to resolve to fixture stem; got:\n{stdout}"
  );
}

#[test]
fn dry_run_with_inspect_dump_json_includes_inspect_frag_dump_json_command() {
  let output = Command::new(env!("CARGO_BIN_EXE_xtask"))
    .current_dir(repo_root())
    .args([
      "page-loop",
      "--fixture",
      "example.com",
      "--inspect-dump-json",
      "--dry-run",
    ])
    .output()
    .expect("run xtask page-loop --inspect-dump-json --dry-run");

  assert!(
    output.status.success(),
    "expected page-loop dry-run to succeed.\nstdout:\n{}\nstderr:\n{}",
    String::from_utf8_lossy(&output.stdout),
    String::from_utf8_lossy(&output.stderr)
  );

  let stdout = String::from_utf8_lossy(&output.stdout);
  let expected_out = repo_root().join("target/page_loop/example.com/inspect");
  let expected_arg = format!("--dump-json {}", expected_out.display());
  assert!(
    stdout.contains("inspect_frag"),
    "expected inspect_frag command in plan; got:\n{stdout}"
  );
  let inspect_line = stdout
    .lines()
    .find(|line| line.contains("scripts/run_limited.sh") && line.contains("inspect_frag") && line.contains("--dump-json"))
    .expect("inspect_frag execution command line should be printed");
  assert!(
    !inspect_line.contains("FASTR_USE_BUNDLED_FONTS=1"),
    "page-loop should not force bundled-only fonts for inspect_frag; got:\n{inspect_line}"
  );
  assert!(
    stdout.contains(&expected_arg),
    "expected inspect_frag to include `{}`; got:\n{stdout}",
    expected_arg
  );
  assert!(
    stdout.contains("--deny-network"),
    "expected inspect_frag to run with --deny-network (fixtures are offline); got:\n{stdout}"
  );
}

#[test]
fn dry_run_with_chrome_and_inspect_dump_json_patches_inspect_frag_html() {
  let output = Command::new(env!("CARGO_BIN_EXE_xtask"))
    .current_dir(repo_root())
    .args([
      "page-loop",
      "--fixture",
      "example.com",
      "--chrome",
      "--inspect-dump-json",
      "--dry-run",
    ])
    .output()
    .expect("run xtask page-loop --chrome --inspect-dump-json --dry-run");

  assert!(
    output.status.success(),
    "expected page-loop dry-run to succeed.\nstdout:\n{}\nstderr:\n{}",
    String::from_utf8_lossy(&output.stdout),
    String::from_utf8_lossy(&output.stderr)
  );

  let stdout = String::from_utf8_lossy(&output.stdout);
  let inspect_line = stdout
    .lines()
    .find(|line| line.contains("run_limited.sh") && line.contains("inspect_frag"))
    .expect("inspect_frag execution command line should be printed");
  assert!(
    inspect_line.contains("--deny-network"),
    "expected inspect_frag to run with --deny-network; got:\n{inspect_line}"
  );
  assert!(
    inspect_line.contains("--patch-html-for-chrome-baseline"),
    "expected inspect_frag to patch HTML when --chrome is enabled; got:\n{inspect_line}"
  );
}

#[test]
fn dry_run_forwards_inspect_filters() {
  let output = Command::new(env!("CARGO_BIN_EXE_xtask"))
    .current_dir(repo_root())
    .args([
      "page-loop",
      "--fixture",
      "example.com",
      "--inspect-dump-json",
      "--inspect-filter-selector",
      "#main",
      "--inspect-filter-id",
      "content",
      "--dry-run",
    ])
    .output()
    .expect("run xtask page-loop --inspect-*filter --dry-run");

  assert!(
    output.status.success(),
    "expected page-loop dry-run to succeed.\nstdout:\n{}\nstderr:\n{}",
    String::from_utf8_lossy(&output.stdout),
    String::from_utf8_lossy(&output.stderr)
  );

  let stdout = String::from_utf8_lossy(&output.stdout);
  assert!(
    stdout.contains("--filter-selector #main"),
    "expected inspect selector filter to be forwarded; got:\n{stdout}"
  );
  assert!(
    stdout.contains("--filter-id content"),
    "expected inspect id filter to be forwarded; got:\n{stdout}"
  );
}

#[test]
fn dry_run_forwards_inspect_custom_property_dump_flags() {
  let output = Command::new(env!("CARGO_BIN_EXE_xtask"))
    .current_dir(repo_root())
    .args([
      "page-loop",
      "--fixture",
      "example.com",
      "--inspect-dump-json",
      "--inspect-dump-custom-properties",
      "--inspect-custom-property-prefix",
      "--tw-",
      "--inspect-custom-property-prefix",
      "--brand-",
      "--inspect-custom-properties-limit",
      "42",
      "--dry-run",
    ])
    .output()
    .expect("run xtask page-loop --inspect-dump-custom-properties --dry-run");

  assert!(
    output.status.success(),
    "expected page-loop dry-run to succeed.\nstdout:\n{}\nstderr:\n{}",
    String::from_utf8_lossy(&output.stdout),
    String::from_utf8_lossy(&output.stderr)
  );

  let stdout = String::from_utf8_lossy(&output.stdout);
  assert!(
    stdout.contains("--dump-custom-properties"),
    "expected inspect_frag custom property dump flag to be forwarded; got:\n{stdout}"
  );
  assert!(
    stdout.contains("--custom-property-prefix=--tw-")
      && stdout.contains("--custom-property-prefix=--brand-"),
    "expected inspect custom-property-prefix filters to be forwarded; got:\n{stdout}"
  );
  assert!(
    stdout.contains("--custom-properties-limit 42"),
    "expected inspect custom-properties-limit to be forwarded; got:\n{stdout}"
  );
}

#[test]
fn from_progress_top_worst_accuracy_prefers_existing_fixtures_and_tiebreaks_perceptual() {
  let temp = tempdir().expect("tempdir");
  let progress_dir = temp.path().join("progress/pages");

  // Both pages tie on diff_percent, so selection should fall back to perceptual distance.
  // Include a "better" page that is missing a fixture to ensure fixture availability is preferred.
  write_progress_file(
    &progress_dir,
    "example.com",
    r#"{"status":"ok","accuracy":{"diff_percent":10.0,"perceptual":0.1}}"#,
  );
  write_progress_file(
    &progress_dir,
    "amazon.com",
    r#"{"status":"ok","accuracy":{"diff_percent":10.0,"perceptual":0.2}}"#,
  );
  write_progress_file(
    &progress_dir,
    "missing-fixture.test",
    r#"{"status":"ok","accuracy":{"diff_percent":99.0,"perceptual":1.0}}"#,
  );

  let output = Command::new(env!("CARGO_BIN_EXE_xtask"))
    .current_dir(repo_root())
    .args([
      "page-loop",
      "--from-progress",
      progress_dir.to_string_lossy().as_ref(),
      "--top-worst-accuracy",
      "1",
      "--dry-run",
    ])
    .output()
    .expect("run xtask page-loop --from-progress --top-worst-accuracy 1 --dry-run");

  assert!(
    output.status.success(),
    "expected page-loop progress selection to succeed.\nstdout:\n{}\nstderr:\n{}",
    String::from_utf8_lossy(&output.stdout),
    String::from_utf8_lossy(&output.stderr)
  );

  let stdout = String::from_utf8_lossy(&output.stdout);
  assert!(
    stdout.contains("fixture: amazon.com"),
    "expected amazon.com to be selected (perceptual tiebreak, and fixture preference over missing-fixture.test); got:\n{stdout}"
  );
}

#[test]
fn from_progress_top_slowest_selects_highest_total_ms() {
  let temp = tempdir().expect("tempdir");
  let progress_dir = temp.path().join("progress/pages");

  write_progress_file(&progress_dir, "example.com", r#"{"status":"ok","total_ms":10.0}"#);
  write_progress_file(&progress_dir, "amazon.com", r#"{"status":"ok","total_ms":50.0}"#);

  let output = Command::new(env!("CARGO_BIN_EXE_xtask"))
    .current_dir(repo_root())
    .args([
      "page-loop",
      "--from-progress",
      progress_dir.to_string_lossy().as_ref(),
      "--top-slowest",
      "1",
      "--dry-run",
    ])
    .output()
    .expect("run xtask page-loop --from-progress --top-slowest 1 --dry-run");

  assert!(
    output.status.success(),
    "expected page-loop progress selection to succeed.\nstdout:\n{}\nstderr:\n{}",
    String::from_utf8_lossy(&output.stdout),
    String::from_utf8_lossy(&output.stderr)
  );

  let stdout = String::from_utf8_lossy(&output.stdout);
  assert!(
    stdout.contains("fixture: amazon.com"),
    "expected amazon.com to be selected as the slowest page; got:\n{stdout}"
  );
}

#[test]
fn from_progress_only_failures_selects_first_failing_stem() {
  let temp = tempdir().expect("tempdir");
  let progress_dir = temp.path().join("progress/pages");

  write_progress_file(&progress_dir, "example.com", r#"{"status":"timeout"}"#);
  write_progress_file(&progress_dir, "amazon.com", r#"{"status":"error"}"#);

  let output = Command::new(env!("CARGO_BIN_EXE_xtask"))
    .current_dir(repo_root())
    .args([
      "page-loop",
      "--from-progress",
      progress_dir.to_string_lossy().as_ref(),
      "--only-failures",
      "--dry-run",
    ])
    .output()
    .expect("run xtask page-loop --from-progress --only-failures --dry-run");

  assert!(
    output.status.success(),
    "expected page-loop progress selection to succeed.\nstdout:\n{}\nstderr:\n{}",
    String::from_utf8_lossy(&output.stdout),
    String::from_utf8_lossy(&output.stderr)
  );

  let stdout = String::from_utf8_lossy(&output.stdout);
  assert!(
    stdout.contains("fixture: amazon.com"),
    "expected amazon.com to be selected as the first failing stem; got:\n{stdout}"
  );
}

#[test]
fn from_progress_hotspot_filters_candidates() {
  let temp = tempdir().expect("tempdir");
  let progress_dir = temp.path().join("progress/pages");

  write_progress_file(
    &progress_dir,
    "amazon.com",
    r#"{"status":"ok","hotspot":"css","accuracy":{"diff_percent":5.0,"perceptual":0.1}}"#,
  );
  write_progress_file(
    &progress_dir,
    "example.com",
    r#"{"status":"ok","hotspot":"layout","accuracy":{"diff_percent":99.0,"perceptual":1.0}}"#,
  );

  let output = Command::new(env!("CARGO_BIN_EXE_xtask"))
    .current_dir(repo_root())
    .args([
      "page-loop",
      "--from-progress",
      progress_dir.to_string_lossy().as_ref(),
      "--top-worst-accuracy",
      "1",
      "--hotspot",
      "CSS",
      "--dry-run",
    ])
    .output()
    .expect("run xtask page-loop --from-progress --hotspot CSS --dry-run");

  assert!(
    output.status.success(),
    "expected page-loop hotspot selection to succeed.\nstdout:\n{}\nstderr:\n{}",
    String::from_utf8_lossy(&output.stdout),
    String::from_utf8_lossy(&output.stderr)
  );

  let stdout = String::from_utf8_lossy(&output.stdout);
  assert!(
    stdout.contains("fixture: amazon.com"),
    "expected hotspot filter to exclude example.com and select amazon.com; got:\n{stdout}"
  );
}

#[test]
fn from_progress_defaults_to_top_worst_accuracy_1() {
  let temp = tempdir().expect("tempdir");
  let progress_dir = temp.path().join("progress/pages");

  write_progress_file(
    &progress_dir,
    "example.com",
    r#"{"status":"ok","accuracy":{"diff_percent":1.0,"perceptual":0.0}}"#,
  );
  write_progress_file(
    &progress_dir,
    "amazon.com",
    r#"{"status":"ok","accuracy":{"diff_percent":2.0,"perceptual":0.0}}"#,
  );

  let output = Command::new(env!("CARGO_BIN_EXE_xtask"))
    .current_dir(repo_root())
    .args([
      "page-loop",
      "--from-progress",
      progress_dir.to_string_lossy().as_ref(),
      "--dry-run",
    ])
    .output()
    .expect("run xtask page-loop --from-progress --dry-run");

  assert!(
    output.status.success(),
    "expected page-loop progress selection to succeed.\nstdout:\n{}\nstderr:\n{}",
    String::from_utf8_lossy(&output.stdout),
    String::from_utf8_lossy(&output.stderr)
  );

  let stdout = String::from_utf8_lossy(&output.stdout);
  assert!(
    stdout.contains("fixture: amazon.com"),
    "expected default selection to pick the worst-accuracy ok page (amazon.com); got:\n{stdout}"
  );
}

#[test]
fn from_progress_errors_when_no_offline_fixture_exists() {
  let temp = tempdir().expect("tempdir");
  let progress_dir = temp.path().join("progress/pages");

  write_progress_file(&progress_dir, "zzz_page_loop_missing_fixture_a", r#"{"status":"timeout"}"#);
  write_progress_file(&progress_dir, "zzz_page_loop_missing_fixture_b", r#"{"status":"error"}"#);

  let output = Command::new(env!("CARGO_BIN_EXE_xtask"))
    .current_dir(repo_root())
    .args([
      "page-loop",
      "--from-progress",
      progress_dir.to_string_lossy().as_ref(),
      "--only-failures",
    ])
    .output()
    .expect("run xtask page-loop --from-progress --only-failures");

  assert!(
    !output.status.success(),
    "expected page-loop to fail when the selected progress page has no offline fixture.\nstdout:\n{}\nstderr:\n{}",
    String::from_utf8_lossy(&output.stdout),
    String::from_utf8_lossy(&output.stderr)
  );

  let stderr = String::from_utf8_lossy(&output.stderr);
  assert!(
    stderr.contains("zzz_page_loop_missing_fixture_a") && stderr.contains("does not have an offline fixture"),
    "expected missing-fixture error to mention the selected stem; got:\n{stderr}"
  );
  assert!(
    stderr.contains("import-page-fixture") || stderr.contains("recapture-page-fixtures"),
    "expected missing-fixture error to include a fixture capture hint; got:\n{stderr}"
  );
}
