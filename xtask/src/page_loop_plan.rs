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
// matches the baseline output.
//
// Note: `--patch-html-for-chrome-baseline` rewrites `.gif` images to a static first-frame PNG for
// determinism (see `cli_utils::fixture_html_patch`), so this mostly matters for other animated image
// formats (e.g. APNG/animated WebP) or animated images referenced outside `<img>/<picture>` tags.
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
  // AVIF is common in real-world fixtures; ensure page-loop builds include AVIF support so
  // offline renders match Chrome baselines and don't treat `image/avif` as a missing image.
  cmd.args(["--features", "avif"]);
  for bin in bins {
    cmd.args(["--bin", bin]);
  }
  cmd
}

#[cfg(test)]
mod build_bins_tests {
  use super::*;
  use std::path::PathBuf;

  fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
      .parent()
      .expect("xtask crate should live under the repository root")
      .to_path_buf()
  }

  #[test]
  fn build_bins_command_enables_avif_feature() {
    let cmd = build_bins_command(&repo_root(), true, &["render_fixtures"]);
    let args: Vec<String> = cmd
      .get_args()
      .map(|arg| arg.to_string_lossy().into_owned())
      .collect();
    assert!(
      args.windows(2).any(|w| w == ["--features", "avif"]),
      "expected build command to enable AVIF support; got {args:?}"
    );
  }
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
  compat_profile: Option<&str>,
  dom_compat: Option<&str>,
  patch_html_for_chrome_baseline: bool,
  write_snapshot: bool,
) -> Command {
  let render_fixtures_exe = render_fixtures_executable(repo_root, debug);
  let mut cmd = cmd::run_limited_command_default(repo_root);
  if std::env::var_os("FASTR_LAYOUT_PARALLEL").is_none() {
    // `FastRenderConfig` defaults to serial layout. Page-loop runners typically render large
    // real-world pages where parallel layout yields major wall-clock improvements, while the
    // `auto` mode keeps smaller fixtures from regressing due to fan-out overhead.
    cmd.env("FASTR_LAYOUT_PARALLEL", "auto");
  }
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
  if let Some(profile) = compat_profile {
    cmd.arg("--compat-profile").arg(profile);
  }
  if let Some(mode) = dom_compat {
    cmd.arg("--dom-compat").arg(mode);
  }
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
  pub compat_profile: Option<&'static str>,
  pub dom_compat: Option<&'static str>,
  pub timeout: u64,
}

pub fn build_inspect_frag_command(
  repo_root: &Path,
  debug: bool,
  args: &InspectFragCommandArgs,
) -> Command {
  let inspect_frag_exe = inspect_frag_executable(repo_root, debug);
  let mut cmd = cmd::run_limited_command_default(repo_root);
  if std::env::var_os("FASTR_LAYOUT_PARALLEL").is_none() {
    cmd.env("FASTR_LAYOUT_PARALLEL", "auto");
  }
  if std::env::var_os("FASTR_DETERMINISTIC_PAINT").is_none() {
    cmd.env("FASTR_DETERMINISTIC_PAINT", "1");
  }
  if std::env::var_os("FASTR_WEB_FONT_WAIT_MS").is_none() {
    // Match `render_fixtures` defaults: wait briefly for `font-display: swap` web fonts so
    // `inspect_frag` overlays/JSON dumps are aligned with both FastRender fixture renders and the
    // Chrome baseline harness (which uses a `--virtual-time-budget`).
    let default_wait_ms = if args.patch_html_for_chrome_baseline {
      "1000"
    } else {
      "500"
    };
    cmd.env("FASTR_WEB_FONT_WAIT_MS", default_wait_ms);
  }
  if args.patch_html_for_chrome_baseline && std::env::var_os("FASTR_TEXT_HINTING").is_none() {
    // Match `render_fixtures` defaults when diffing against Chrome baselines.
    cmd.env("FASTR_TEXT_HINTING", "1");
  }
  if args.patch_html_for_chrome_baseline
    && std::env::var_os("FASTR_TEXT_SNAP_GLYPH_POSITIONS").is_none()
  {
    // Chrome baseline harness disables font subpixel positioning; keep inspect output aligned with
    // `render_fixtures` fixture-chrome mode defaults.
    cmd.env("FASTR_TEXT_SNAP_GLYPH_POSITIONS", "1");
  }
  if args.patch_html_for_chrome_baseline && std::env::var_os("FASTR_TEXT_SUBPIXEL_AA").is_none() {
    // Keep inspect overlays aligned with `render_fixtures` fixture-chrome mode defaults.
    cmd.env("FASTR_TEXT_SUBPIXEL_AA", "0");
  }
  if args.patch_html_for_chrome_baseline
    && std::env::var_os("FASTR_TEXT_SUBPIXEL_AA_GAMMA").is_none()
  {
    cmd.env("FASTR_TEXT_SUBPIXEL_AA_GAMMA", "1.0");
  }
  if args.patch_html_for_chrome_baseline && std::env::var_os("FASTR_HIDE_SCROLLBARS").is_none() {
    // Match `render_fixtures --patch-html-for-chrome-baseline` / Chrome baseline harness behavior
    // (no reserved scrollbar gutters).
    cmd.env("FASTR_HIDE_SCROLLBARS", "1");
  }
  if args.patch_html_for_chrome_baseline
    && std::env::var_os("FASTR_COMPAT_REPLACED_MAX_WIDTH_100").is_none()
  {
    // Chrome's UA stylesheet does not apply a `max-width: 100%` compatibility default to replaced
    // elements. Disable FastRender's non-standard default in fixture-chrome mode so diffs reflect
    // renderer behavior rather than differing UA defaults.
    cmd.env("FASTR_COMPAT_REPLACED_MAX_WIDTH_100", "0");
  }
  cmd.arg(inspect_frag_exe);
  cmd.arg(&args.fixture_html);
  // `page-loop` renders offline fixtures; forbid HTTP(S) so inspect runs don't hang on stray remote
  // URLs that should have been bundled into the fixture directory.
  cmd.arg("--deny-network");
  if let Some(profile) = args.compat_profile {
    cmd.arg("--compat-profile").arg(profile);
  }
  if let Some(mode) = args.dom_compat {
    cmd.arg("--dom-compat").arg(mode);
  }
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

#[cfg(test)]
mod tests {
  use super::*;
  use std::collections::HashMap;
  use std::ffi::OsString;
  use std::sync::{Mutex, OnceLock};

  static ENV_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

  struct EnvVarRestore {
    key: &'static str,
    prev: Option<OsString>,
  }

  impl EnvVarRestore {
    fn set(key: &'static str, value: Option<&str>) -> Self {
      let prev = std::env::var_os(key);
      match value {
        Some(value) => std::env::set_var(key, value),
        None => std::env::remove_var(key),
      }
      Self { key, prev }
    }
  }

  impl Drop for EnvVarRestore {
    fn drop(&mut self) {
      match self.prev.take() {
        Some(value) => std::env::set_var(self.key, value),
        None => std::env::remove_var(self.key),
      }
    }
  }

  fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
      .parent()
      .expect("xtask crate should live under the repository root")
      .to_path_buf()
  }

  fn default_args(patch: bool) -> InspectFragCommandArgs {
    InspectFragCommandArgs {
      fixture_html: PathBuf::from("fixture.html"),
      overlay_png: None,
      dump_json_dir: None,
      filter_selector: None,
      filter_id: None,
      dump_custom_properties: false,
      custom_property_prefix: Vec::new(),
      custom_properties_limit: None,
      patch_html_for_chrome_baseline: patch,
      viewport: (1040, 1240),
      dpr: 1.0,
      media: "screen".to_string(),
      compat_profile: None,
      dom_compat: None,
      timeout: 10,
    }
  }

  fn env_map(cmd: &Command) -> HashMap<String, Option<String>> {
    cmd
      .get_envs()
      .map(|(k, v)| {
        (
          k.to_string_lossy().into_owned(),
          v.map(|v| v.to_string_lossy().into_owned()),
        )
      })
      .collect()
  }

  #[test]
  fn build_inspect_frag_command_defaults_web_font_wait_ms() {
    let _lock = ENV_LOCK.get_or_init(|| Mutex::new(())).lock().unwrap();
    let _restore = EnvVarRestore::set("FASTR_WEB_FONT_WAIT_MS", None);

    let cmd = build_inspect_frag_command(&repo_root(), true, &default_args(true));
    let envs = env_map(&cmd);
    assert_eq!(
      envs
        .get("FASTR_WEB_FONT_WAIT_MS")
        .and_then(|v| v.as_deref()),
      Some("1000")
    );
  }

  #[test]
  fn build_inspect_frag_command_does_not_override_web_font_wait_ms() {
    let _lock = ENV_LOCK.get_or_init(|| Mutex::new(())).lock().unwrap();
    let _restore = EnvVarRestore::set("FASTR_WEB_FONT_WAIT_MS", Some("123"));

    let cmd = build_inspect_frag_command(&repo_root(), true, &default_args(true));
    let envs = env_map(&cmd);
    assert!(
      !envs.contains_key("FASTR_WEB_FONT_WAIT_MS"),
      "expected inspect_frag command to inherit FASTR_WEB_FONT_WAIT_MS when explicitly set"
    );
  }
}
