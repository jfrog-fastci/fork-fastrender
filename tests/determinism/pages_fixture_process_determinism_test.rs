use crate::r#ref::compare::{compare_images, load_png_from_bytes, CompareConfig};
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use tempfile::TempDir;

const ENV_TIMEOUT_SECS: &str = "FASTR_DETERMINISM_TIMEOUT_SECS";
const ENV_VIEWPORT: &str = "FASTR_DETERMINISM_VIEWPORT";
const DPR: &str = "1.0";
const MEDIA: &str = "screen";
const JOBS: &str = "1";

fn manifest_dir() -> PathBuf {
  PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn artifacts_root() -> PathBuf {
  manifest_dir().join("target/determinism_diffs")
}

fn parse_timeout_secs_from_env() -> u64 {
  match env::var(ENV_TIMEOUT_SECS) {
    Ok(value) => {
      let trimmed = value.trim();
      if trimmed.is_empty() {
        panic!("{ENV_TIMEOUT_SECS} must not be empty");
      }
      let parsed: u64 = trimmed
        .parse()
        .unwrap_or_else(|e| panic!("invalid {ENV_TIMEOUT_SECS}={trimmed:?}: {e}"));
      assert!(parsed > 0, "{ENV_TIMEOUT_SECS} must be > 0");
      parsed
    }
    Err(env::VarError::NotPresent) => 45,
    Err(e) => panic!("failed to read {ENV_TIMEOUT_SECS}: {e}"),
  }
}

fn parse_viewport_str(value: &str) -> (u32, u32) {
  let trimmed = value.trim();
  let (w_str, h_str) = trimmed
    .split_once('x')
    .or_else(|| trimmed.split_once('X'))
    .unwrap_or_else(|| panic!("viewport must be formatted as WxH (got {trimmed:?})"));

  let w: u32 = w_str
    .trim()
    .parse()
    .unwrap_or_else(|e| panic!("invalid viewport width {w_str:?}: {e}"));
  let h: u32 = h_str
    .trim()
    .parse()
    .unwrap_or_else(|e| panic!("invalid viewport height {h_str:?}: {e}"));

  assert!(w > 0 && h > 0, "viewport must be non-zero (got {w}x{h})");
  (w, h)
}

fn parse_viewport_arg_from_env() -> String {
  match env::var(ENV_VIEWPORT) {
    Ok(value) => {
      let (w, h) = parse_viewport_str(&value);
      format!("{w}x{h}")
    }
    Err(env::VarError::NotPresent) => {
      // Keep this test conservative by default: the determinism signal comes from rendering a
      // blur-heavy fixture twice across processes, so total runtime scales with pixel area.
      // Developers can opt into a larger viewport via `FASTR_DETERMINISM_VIEWPORT`.
      "600x480".to_string()
    }
    Err(e) => panic!("failed to read {ENV_VIEWPORT}: {e}"),
  }
}

fn run_render_fixtures(fixtures: &[&str], out_dir: &Path, viewport: &str, timeout_secs: u64) {
  let fixtures_arg = fixtures.join(",");
  let timeout_arg = timeout_secs.to_string();
  let status = Command::new(env!("CARGO_BIN_EXE_render_fixtures"))
    .current_dir(manifest_dir())
    .args([
      "--fixtures",
      &fixtures_arg,
      "--jobs",
      JOBS,
      "--out-dir",
      out_dir.to_str().expect("out-dir utf8"),
      "--timeout",
      &timeout_arg,
      "--viewport",
      viewport,
      "--dpr",
      DPR,
      "--media",
      MEDIA,
    ])
    .status()
    .expect("spawn render_fixtures");

  assert!(
    status.success(),
    "render_fixtures failed for fixtures [{fixtures_arg}] (viewport={viewport}, timeout={timeout_secs}s, out_dir={})",
    out_dir.display()
  );
}

fn run_render_fixture_with_snapshot(
  stem: &str,
  out_dir: &Path,
  viewport: &str,
  timeout_secs: u64,
) -> Result<(), String> {
  let timeout_arg = timeout_secs.to_string();
  let status = Command::new(env!("CARGO_BIN_EXE_render_fixtures"))
    .current_dir(manifest_dir())
    .args([
      "--fixtures",
      stem,
      "--jobs",
      JOBS,
      "--out-dir",
      out_dir.to_str().expect("out-dir utf8"),
      "--timeout",
      &timeout_arg,
      "--viewport",
      viewport,
      "--dpr",
      DPR,
      "--media",
      MEDIA,
      "--write-snapshot",
    ])
    .status()
    .map_err(|e| format!("failed to spawn render_fixtures --write-snapshot: {e}"))?;

  if status.success() {
    Ok(())
  } else {
    Err(format!(
      "render_fixtures --write-snapshot failed for fixture {stem} (viewport={viewport}, timeout={timeout_secs}s, out_dir={})",
      out_dir.display()
    ))
  }
}

fn run_diff_snapshots(before_dir: &Path, after_dir: &Path, out_dir: &Path) -> Result<(), String> {
  let json_path = out_dir.join("diff_snapshots.json");
  let html_path = out_dir.join("diff_snapshots.html");
  let status = Command::new(env!("CARGO_BIN_EXE_diff_snapshots"))
    .current_dir(manifest_dir())
    .args([
      "--before",
      before_dir.to_str().expect("before-dir utf8"),
      "--after",
      after_dir.to_str().expect("after-dir utf8"),
      "--json",
      json_path.to_str().expect("json path utf8"),
      "--html",
      html_path.to_str().expect("html path utf8"),
    ])
    .status()
    .map_err(|e| format!("failed to spawn diff_snapshots: {e}"))?;

  if status.success() {
    Ok(())
  } else {
    Err(format!(
      "diff_snapshots failed (before={}, after={})",
      before_dir.display(),
      after_dir.display()
    ))
  }
}

fn compare_fixture_png(
  stem: &str,
  run1_dir: &Path,
  run2_dir: &Path,
  viewport: &str,
  timeout_secs: u64,
) {
  let png1_path = run1_dir.join(format!("{stem}.png"));
  let png2_path = run2_dir.join(format!("{stem}.png"));
  let png1 =
    fs::read(&png1_path).unwrap_or_else(|e| panic!("failed to read {}: {e}", png1_path.display()));
  let png2 =
    fs::read(&png2_path).unwrap_or_else(|e| panic!("failed to read {}: {e}", png2_path.display()));

  let pix1 = load_png_from_bytes(&png1)
    .unwrap_or_else(|e| panic!("failed to decode {}: {e}", png1_path.display()));
  let pix2 = load_png_from_bytes(&png2)
    .unwrap_or_else(|e| panic!("failed to decode {}: {e}", png2_path.display()));

  if pix1.width() == pix2.width() && pix1.height() == pix2.height() && pix1.data() == pix2.data() {
    return;
  }

  let diff = compare_images(&pix1, &pix2, &CompareConfig::strict());

  let artifact_dir = artifacts_root().join(stem);
  if artifact_dir.exists() {
    fs::remove_dir_all(&artifact_dir)
      .unwrap_or_else(|e| panic!("failed to remove {}: {e}", artifact_dir.display()));
  }
  fs::create_dir_all(&artifact_dir)
    .unwrap_or_else(|e| panic!("failed to create {}: {e}", artifact_dir.display()));

  let run1_out = artifact_dir.join(format!("{stem}_run1.png"));
  let run2_out = artifact_dir.join(format!("{stem}_run2.png"));
  let diff_out = artifact_dir.join(format!("{stem}_diff.png"));

  fs::write(&run1_out, &png1)
    .unwrap_or_else(|e| panic!("failed to write {}: {e}", run1_out.display()));
  fs::write(&run2_out, &png2)
    .unwrap_or_else(|e| panic!("failed to write {}: {e}", run2_out.display()));

  let diff_written = if diff.diff_image.is_some() {
    diff
      .save_diff_image(&diff_out)
      .unwrap_or_else(|e| panic!("failed to write {}: {e}", diff_out.display()));
    true
  } else {
    false
  };

  // Capture pipeline snapshots and generate a stage-level snapshot diff report to make
  // process-level nondeterminism actionable.
  let snapshot_run1_out = artifact_dir.join("run1");
  let snapshot_run2_out = artifact_dir.join("run2");
  fs::create_dir_all(&snapshot_run1_out).expect("create snapshot run1 dir");
  fs::create_dir_all(&snapshot_run2_out).expect("create snapshot run2 dir");

  let snapshot_before_dir = snapshot_run1_out.join(stem);
  let snapshot_after_dir = snapshot_run2_out.join(stem);
  let mut snapshot_error = None::<String>;

  if let Err(err) =
    run_render_fixture_with_snapshot(stem, &snapshot_run1_out, viewport, timeout_secs)
  {
    snapshot_error = Some(format!("run1 snapshot capture failed: {err}"));
  } else if let Err(err) =
    run_render_fixture_with_snapshot(stem, &snapshot_run2_out, viewport, timeout_secs)
  {
    snapshot_error = Some(format!("run2 snapshot capture failed: {err}"));
  } else {
    for (label, dir) in [
      ("run1", &snapshot_before_dir),
      ("run2", &snapshot_after_dir),
    ] {
      for required in ["snapshot.json", "diagnostics.json"] {
        let path = dir.join(required);
        if !path.is_file() {
          snapshot_error = Some(format!(
            "missing {required} in {label} snapshot dir {}",
            dir.display()
          ));
          break;
        }
      }
      if snapshot_error.is_some() {
        break;
      }
    }
  }

  if snapshot_error.is_none() {
    // Make diff_snapshots link the exact pixel mismatch.
    if let Err(err) = fs::write(snapshot_before_dir.join("render.png"), &png1) {
      snapshot_error = Some(format!("failed to write run1 render.png: {err}"));
    } else if let Err(err) = fs::write(snapshot_after_dir.join("render.png"), &png2) {
      snapshot_error = Some(format!("failed to write run2 render.png: {err}"));
    } else if let Err(err) =
      run_diff_snapshots(&snapshot_before_dir, &snapshot_after_dir, &artifact_dir)
    {
      snapshot_error = Some(err);
    }
  }

  let mut message = String::new();
  message.push_str(&format!(
    "Fixture '{stem}' rendered nondeterministically across processes.\n\nPixel diff: {}\n\nArtifacts:\n  run1: {}\n  run2: {}\n",
    diff.summary(),
    run1_out.display(),
    run2_out.display(),
  ));
  if diff_written {
    message.push_str(&format!("  diff: {}\n", diff_out.display()));
  } else {
    message.push_str("  diff: (not generated; likely dimension mismatch)\n");
  }

  message.push_str("\nSnapshots:\n");
  message.push_str(&format!("  run1: {}\n", snapshot_before_dir.display()));
  message.push_str(&format!("  run2: {}\n", snapshot_after_dir.display()));
  message.push_str("\nSnapshot stage diff:\n");
  message.push_str(&format!(
    "  html: {}\n  json: {}\n",
    artifact_dir.join("diff_snapshots.html").display(),
    artifact_dir.join("diff_snapshots.json").display()
  ));
  if let Some(err) = snapshot_error {
    message.push_str(&format!("\nSnapshot capture failed:\n{err}\n"));
  }

  panic!("{message}");
}

#[test]
fn pages_fixture_process_determinism_test() {
  let fixtures = ["preserve_3d_stack", "filter_backdrop_scene"];

  let viewport = parse_viewport_arg_from_env();
  let timeout_secs = parse_timeout_secs_from_env();

  let run1 = TempDir::new().expect("tempdir run1");
  let run2 = TempDir::new().expect("tempdir run2");

  run_render_fixtures(&fixtures, run1.path(), &viewport, timeout_secs);
  run_render_fixtures(&fixtures, run2.path(), &viewport, timeout_secs);

  for stem in fixtures {
    compare_fixture_png(stem, run1.path(), run2.path(), &viewport, timeout_secs);
  }
}
