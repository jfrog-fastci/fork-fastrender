//! `xtask page-loop` command planning helpers.
//!
//! Historically `page-loop` spawned renderer binaries via the Cargo wrapper
//! (`bash scripts/cargo_agent.sh run --bin …`). That keeps Cargo holding the
//! target-dir lock (and some global locks) for the entire duration of the render, which makes it
//! easy for multi-agent hosts to stall when several workers run page-loop workflows at once.
//!
//! To keep the workflow snappy and reliable we:
//! - build the required renderer binaries once via `scripts/cargo_agent.sh build --bin …` (using
//!   `--release` unless `xtask page-loop --debug` was requested),
//! - execute the resulting `target/{debug,release}/<bin>` directly,
//! - always wrap executions with `scripts/run_limited.sh` so the renderer stays within a safe
//!   address-space limit.

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::cmd;

// Chrome baseline screenshots are captured via `--virtual-time-budget=5000`. Animated images start
// animating slightly after the budget begins (decode/paint delay), so sample at an offset that
// matches the baseline output. Keep this value away from common frame boundaries (e.g. 30ms GIFs)
// so we don't systematically land exactly on a frame transition.
const CHROME_BASELINE_ANIMATION_TIME_MS: &str = "4940";

fn resolve_cargo_target_dir(repo_root: &Path, cargo_target_dir: Option<&Path>) -> PathBuf {
  match cargo_target_dir {
    Some(path) if path.as_os_str().is_empty() => repo_root.join("target"),
    Some(path) if path.is_absolute() => path.to_path_buf(),
    Some(path) => repo_root.join(path),
    None => repo_root.join("target"),
  }
}

fn cargo_target_dir(repo_root: &Path) -> PathBuf {
  let cargo_target_dir = std::env::var_os("CARGO_TARGET_DIR").map(PathBuf::from);
  resolve_cargo_target_dir(repo_root, cargo_target_dir.as_deref())
}

fn executable(repo_root: &Path, debug: bool, bin_name: &str) -> PathBuf {
  let profile = if debug { "debug" } else { "release" };
  cargo_target_dir(repo_root)
    .join(profile)
    .join(format!("{bin_name}{}", std::env::consts::EXE_SUFFIX))
}

#[inline]
pub fn render_fixtures_executable(repo_root: &Path, debug: bool) -> PathBuf {
  executable(repo_root, debug, "render_fixtures")
}

#[inline]
pub fn inspect_frag_executable(repo_root: &Path, debug: bool) -> PathBuf {
  executable(repo_root, debug, "inspect_frag")
}

#[inline]
pub fn diff_renders_executable(repo_root: &Path, debug: bool) -> PathBuf {
  executable(repo_root, debug, "diff_renders")
}

pub fn build_bins_command(repo_root: &Path, debug: bool, bins: &[&str]) -> Command {
  let mut cmd = cmd::cargo_agent_command(repo_root);
  cmd.current_dir(repo_root);
  cmd.arg("build");
  if !debug {
    cmd.arg("--release");
  }
  for bin in bins {
    cmd.args(["--bin", bin]);
  }
  cmd
}

#[allow(clippy::too_many_arguments)]
pub fn build_render_fixtures_command(
  repo_root: &Path,
  debug: bool,
  fixtures_dir: &Path,
  out_dir: &Path,
  fixture_stem: &str,
  jobs: usize,
  viewport: (u32, u32),
  dpr: f32,
  media: &str,
  timeout: u64,
  patch_html_for_chrome_baseline: bool,
  write_snapshot: bool,
) -> Command {
  let render_fixtures_exe = render_fixtures_executable(repo_root, debug);
  let mut cmd = cmd::run_limited_command_default(repo_root);
  cmd.arg(render_fixtures_exe);
  cmd.arg("--fixtures-dir").arg(fixtures_dir);
  cmd.arg("--out-dir").arg(out_dir);
  cmd.arg("--fixtures").arg(fixture_stem);
  cmd.arg("--jobs").arg(jobs.to_string());
  cmd
    .arg("--viewport")
    .arg(format!("{}x{}", viewport.0, viewport.1));
  cmd.arg("--dpr").arg(dpr.to_string());
  cmd.arg("--media").arg(media);
  cmd.arg("--timeout").arg(timeout.to_string());
  if patch_html_for_chrome_baseline {
    // `chrome-baseline-fixtures` always renders a patched HTML variant (forces light-mode, disables
    // JS/animations via CSP + style injection). When diffing against Chrome, render the same patch
    // via `render_fixtures` so the resulting report reflects renderer differences rather than the
    // harness modifications.
    cmd.arg("--patch-html-for-chrome-baseline");
    // Chrome baselines use system fonts for generic families like `serif`/`sans-serif`, which can't
    // be redirected via `@font-face` aliases. Enable system font discovery on the FastRender side so
    // chrome diffs aren't dominated by generic font metric mismatches.
    cmd.arg("--system-fonts");
    // Chrome baselines are captured with `--virtual-time-budget=5000ms`, which advances animated
    // images (e.g. GIFs) even though CSS animations/transitions are disabled by the baseline patch.
    // Sample at the timestamp that matches Chrome's screenshot output.
    cmd
      .arg("--animation-time-ms")
      .arg(CHROME_BASELINE_ANIMATION_TIME_MS);
  }
  if write_snapshot {
    cmd.arg("--write-snapshot");
  }
  cmd.current_dir(repo_root);
  cmd
}

#[derive(Debug, Clone)]
pub struct InspectFragCommandArgs {
  pub fixture_html: PathBuf,
  pub overlay_png: Option<PathBuf>,
  pub dump_json_dir: Option<PathBuf>,
  pub filter_selector: Option<String>,
  pub filter_id: Option<String>,
  pub dump_custom_properties: bool,
  pub custom_property_prefix: Vec<String>,
  pub custom_properties_limit: Option<usize>,
  pub patch_html_for_chrome_baseline: bool,
  pub viewport: (u32, u32),
  pub dpr: f32,
  pub media: String,
  pub timeout: u64,
}

pub fn build_inspect_frag_command(
  repo_root: &Path,
  debug: bool,
  args: &InspectFragCommandArgs,
) -> Command {
  let inspect_frag_exe = inspect_frag_executable(repo_root, debug);
  let mut cmd = cmd::run_limited_command_default(repo_root);
  if std::env::var_os("FASTR_DETERMINISTIC_PAINT").is_none() {
    cmd.env("FASTR_DETERMINISTIC_PAINT", "1");
  }
  if std::env::var_os("FASTR_WEB_FONT_WAIT_MS").is_none() {
    // Match `render_fixtures` defaults: when diffing against Chrome baselines we render the
    // pre-swap state for `font-display: swap` web fonts (unless the caller overrides
    // `FASTR_WEB_FONT_WAIT_MS` explicitly).
    let default_wait_ms = if args.patch_html_for_chrome_baseline { "0" } else { "500" };
    cmd.env("FASTR_WEB_FONT_WAIT_MS", default_wait_ms);
  }
  if args.patch_html_for_chrome_baseline && std::env::var_os("FASTR_TEXT_SUBPIXEL_AA").is_none() {
    // Keep inspect overlays aligned with `render_fixtures` fixture-chrome mode defaults.
    cmd.env("FASTR_TEXT_SUBPIXEL_AA", "1");
  }
  if args.patch_html_for_chrome_baseline
    && std::env::var_os("FASTR_TEXT_SUBPIXEL_AA_GAMMA").is_none()
  {
    cmd.env("FASTR_TEXT_SUBPIXEL_AA_GAMMA", "1.4");
  }
  if args.patch_html_for_chrome_baseline && std::env::var_os("FASTR_HIDE_SCROLLBARS").is_none() {
    // Match `render_fixtures --patch-html-for-chrome-baseline` / Chrome baseline harness behavior
    // (no reserved scrollbar gutters).
    cmd.env("FASTR_HIDE_SCROLLBARS", "1");
  }
  cmd.arg(inspect_frag_exe);
  cmd.arg(&args.fixture_html);
  // `page-loop` renders offline fixtures; forbid HTTP(S) so inspect runs don't hang on stray remote
  // URLs that should have been bundled into the fixture directory.
  cmd.arg("--deny-network");
  if args.patch_html_for_chrome_baseline {
    cmd.arg("--patch-html-for-chrome-baseline");
    // Keep inspect output aligned with the `render_fixtures` step when diffing against Chrome:
    // generic font families in fixtures resolve via the host's system font database in Chrome.
    cmd.arg("--system-fonts");
    cmd
      .arg("--animation-time-ms")
      .arg(CHROME_BASELINE_ANIMATION_TIME_MS);
  }
  if let Some(overlay) = args.overlay_png.as_ref() {
    cmd.arg("--render-overlay").arg(overlay);
  }
  if let Some(dir) = args.dump_json_dir.as_ref() {
    cmd.arg("--dump-json").arg(dir);
  }
  if let Some(selector) = args.filter_selector.as_deref() {
    cmd.arg("--filter-selector").arg(selector);
  }
  if let Some(id) = args.filter_id.as_deref() {
    cmd.arg("--filter-id").arg(id);
  }
  if args.dump_custom_properties {
    cmd.arg("--dump-custom-properties");
    for prefix in &args.custom_property_prefix {
      // `--custom-property-prefix` values often start with `--` (since CSS custom properties do).
      // `inspect_frag` (clap) treats bare `--foo` tokens as flags unless passed in `--flag=value`
      // form, so use the equals-sign style to avoid requiring callers to do the same.
      cmd.arg(format!("--custom-property-prefix={prefix}"));
    }
    if let Some(limit) = args.custom_properties_limit {
      cmd.arg("--custom-properties-limit").arg(limit.to_string());
    }
  }
  cmd
    .arg("--viewport")
    .arg(format!("{}x{}", args.viewport.0, args.viewport.1));
  cmd.arg("--dpr").arg(args.dpr.to_string());
  cmd.arg("--media").arg(&args.media);
  cmd.arg("--timeout").arg(args.timeout.to_string());
  cmd.current_dir(repo_root);
  cmd
}
