use anyhow::{anyhow, bail, Context, Result};
use clap::{Args, ValueEnum};
use crate::fixture_html_patch;
use image::{GenericImage, ImageBuffer, Rgba};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};
use tempfile::TempDir;
use url::Url;
use walkdir::WalkDir;

/// Minimum `RLIMIT_AS` (virtual address space) required for Chrome to start reliably in our CI-like
/// environments.
///
/// `scripts/cargo_agent.sh` enforces a 64GiB address-space cap via `scripts/run_limited.sh` by
/// default (96GiB for `bash scripts/cargo_agent.sh xtask ...`). Unfortunately, recent Chrome builds reserve >64GiB of
/// virtual address space at startup
/// (even for trivial pages), which trips Oilpan's OOM guard and causes headless Chrome to hang.
///
/// To keep `cargo_agent`'s default safety guardrails while still allowing Chrome baselines, we
/// temporarily raise `RLIMIT_AS` for the duration of the `chrome` spawn (then immediately restore
/// the original limits in the parent process).
#[cfg(target_os = "linux")]
const CHROME_MIN_RLIMIT_AS_BYTES: u64 = 96 * 1024 * 1024 * 1024;

#[cfg(target_os = "linux")]
struct ChromeRlimitAsGuard {
  previous: libc::rlimit,
}

#[cfg(target_os = "linux")]
impl ChromeRlimitAsGuard {
  fn ensure_min_bytes(min_bytes: u64) -> io::Result<Option<Self>> {
    let mut current = libc::rlimit {
      rlim_cur: 0,
      rlim_max: 0,
    };
    // SAFETY: `getrlimit` writes to `current` when the pointer is valid.
    let rc = unsafe { libc::getrlimit(libc::RLIMIT_AS, &mut current) };
    if rc != 0 {
      return Err(io::Error::last_os_error());
    }

    // Nothing to do when already unlimited.
    if current.rlim_cur == libc::RLIM_INFINITY && current.rlim_max == libc::RLIM_INFINITY {
      return Ok(None);
    }

    let min_rlim: libc::rlim_t = min_bytes.try_into().map_err(|_| {
      io::Error::new(
        io::ErrorKind::InvalidInput,
        "min_bytes too large for rlim_t",
      )
    })?;

    let needs_cur = current.rlim_cur != libc::RLIM_INFINITY && current.rlim_cur < min_rlim;
    let needs_max = current.rlim_max != libc::RLIM_INFINITY && current.rlim_max < min_rlim;
    if !needs_cur && !needs_max {
      return Ok(None);
    }

    // Raise the soft limit to at least `min_bytes`. Only raise the hard limit when strictly
    // necessary (some environments disallow raising the hard limit without privileges).
    let desired_max = if current.rlim_max == libc::RLIM_INFINITY {
      libc::RLIM_INFINITY
    } else {
      current.rlim_max.max(min_rlim)
    };
    let desired_cur = if desired_max == libc::RLIM_INFINITY {
      if current.rlim_cur == libc::RLIM_INFINITY {
        libc::RLIM_INFINITY
      } else {
        current.rlim_cur.max(min_rlim)
      }
    } else if current.rlim_cur == libc::RLIM_INFINITY {
      // Be defensive: ensure rlim_cur is never greater than the hard limit.
      desired_max
    } else {
      current.rlim_cur.max(min_rlim).min(desired_max)
    };

    let desired = libc::rlimit {
      rlim_cur: desired_cur,
      rlim_max: desired_max,
    };

    // SAFETY: `setrlimit` is a process-global syscall. We pass a properly-initialized `rlimit`.
    let rc = unsafe { libc::setrlimit(libc::RLIMIT_AS, &desired) };
    if rc != 0 {
      return Err(io::Error::last_os_error());
    }

    Ok(Some(Self { previous: current }))
  }

  fn previous_bytes(&self) -> (u64, u64) {
    (self.previous.rlim_cur as u64, self.previous.rlim_max as u64)
  }
}

#[cfg(target_os = "linux")]
impl Drop for ChromeRlimitAsGuard {
  fn drop(&mut self) {
    // SAFETY: `setrlimit` is a process-global syscall. We pass a properly-initialized `rlimit`.
    let rc = unsafe { libc::setrlimit(libc::RLIMIT_AS, &self.previous) };
    if rc != 0 {
      eprintln!(
        "warning: failed to restore RLIMIT_AS after spawning chrome: {}",
        io::Error::last_os_error()
      );
    }
  }
}


/// When Chrome runs in headless mode, `--window-size=WxH` sets the outer window size, but the
/// layout viewport (what CSS `position: fixed` / `100vh` uses) is consistently shorter by ~87px
/// (default).
///
/// This manifests as a white bar at the bottom of `--screenshot` outputs. To make the captured PNG
/// match the requested viewport, we:
/// 1) add this padding to the window height passed to Chrome, then
/// 2) crop the resulting screenshot back down to the requested size.
///
/// The default value is shared with `scripts/chrome_baseline.sh` (used by pageset_progress).
///
/// Override via `HEADLESS_WINDOW_VIEWPORT_HEIGHT_PAD_PX` if you see persistent viewport mismatch
/// artifacts on a different Chrome build/OS.
///
/// See `docs/notes/chrome-headless-viewport-padding.md`.
const DEFAULT_HEADLESS_WINDOW_VIEWPORT_HEIGHT_PAD_PX: u32 = 87;

/// Default `--virtual-time-budget` (ms) used for Chrome fixture screenshots when JS is disabled.
///
/// With JS disabled (via CSP injection), Chrome's built-in `--screenshot` flow can capture before
/// late-loading images have decoded/painted, resulting in blank thumbnail slots on real-world
/// fixtures. A small virtual-time budget gives the page enough breathing room to paint those images
/// without requiring JS.
const DEFAULT_HEADLESS_SCREENSHOT_VIRTUAL_TIME_BUDGET_MS: u64 = 5_000;

fn write_js_disabled_chrome_preferences(profile_dir: &Path) -> Result<()> {
  // `--disable-javascript` is not a stable/portable Chrome CLI flag, but Chrome does honor profile
  // content settings stored in the user-data-dir.
  //
  // We want `--js off` to behave like a user disabling JavaScript in browser settings so that:
  // - scripts don't execute, and
  // - `<noscript>` fallback markup is parsed/rendered (instead of being treated as raw text).
  //
  // This makes fixtures like pinterest.com (which gate their UI behind `<noscript>`) render
  // meaningful content in the baseline instead of a blank page.
  let default_dir = profile_dir.join("Default");
  fs::create_dir_all(&default_dir).with_context(|| {
    format!(
      "create chrome profile Default dir {}",
      default_dir.display()
    )
  })?;
  let preferences_path = default_dir.join("Preferences");
  let prefs = serde_json::json!({
    "profile": {
      // Cover both managed + default settings; Chrome prefers managed settings when present.
      "managed_default_content_settings": { "javascript": 2 },
      "default_content_setting_values": { "javascript": 2 },
    }
  });
  fs::write(
    &preferences_path,
    serde_json::to_vec(&prefs).expect("serialize chrome prefs"),
  )
  .with_context(|| format!("write chrome preferences {}", preferences_path.display()))?;
  Ok(())
}

fn headless_window_viewport_height_pad_px() -> Result<u32> {
  match std::env::var("HEADLESS_WINDOW_VIEWPORT_HEIGHT_PAD_PX") {
    Ok(raw) => {
      let raw = raw.trim().to_string();
      if raw.is_empty() {
        return Ok(DEFAULT_HEADLESS_WINDOW_VIEWPORT_HEIGHT_PAD_PX);
      }
      let value = raw.parse::<u32>().with_context(|| {
        format!(
          "invalid HEADLESS_WINDOW_VIEWPORT_HEIGHT_PAD_PX={raw} (expected a non-negative integer)"
        )
      })?;
      Ok(value)
    }
    Err(std::env::VarError::NotPresent) => Ok(DEFAULT_HEADLESS_WINDOW_VIEWPORT_HEIGHT_PAD_PX),
    Err(err) => bail!("failed to read HEADLESS_WINDOW_VIEWPORT_HEIGHT_PAD_PX: {err}"),
  }
}

#[derive(Args, Debug)]
pub struct ChromeBaselineFixturesArgs {
  /// Root directory containing fixture directories (each must contain an index.html).
  #[arg(
    long,
    visible_aliases = ["fixtures-dir", "fixtures-root"],
    default_value = "tests/pages/fixtures",
    value_name = "DIR"
  )]
  fixture_dir: PathBuf,

  /// Directory to write PNGs/logs into.
  #[arg(
    long,
    default_value = "target/chrome_fixture_renders",
    value_name = "DIR"
  )]
  out_dir: PathBuf,

  /// Only render listed fixture directory names (comma-separated).
  #[arg(long, alias = "only", value_delimiter = ',', value_name = "STEM,...")]
  fixtures: Option<Vec<String>>,

  /// Positional fixture filters (equivalent to `--fixtures`).
  #[arg(value_name = "FIXTURE", conflicts_with = "fixtures", num_args = 0..)]
  fixtures_pos: Vec<String>,

  /// Process only a deterministic shard of discovered fixtures (index/total, 0-based).
  #[arg(long, value_parser = crate::parse_shard)]
  shard: Option<(usize, usize)>,

  /// Chrome/Chromium binary to run (defaults to auto-detect; can also be set via CHROME_BIN).
  #[arg(long, value_name = "PATH")]
  chrome: Option<PathBuf>,

  /// Directory to search for a `chrome`/`chromium` binary.
  ///
  /// When provided, auto-detection via PATH is disabled. This is primarily intended for tests.
  #[arg(long, value_name = "DIR")]
  chrome_dir: Option<PathBuf>,

  /// Viewport size as WxH (e.g. 1040x1240).
  #[arg(long, value_parser = crate::parse_viewport, default_value = "1040x1240")]
  viewport: (u32, u32),

  /// Device pixel ratio for media queries/srcset.
  #[arg(long, default_value_t = 1.0)]
  dpr: f32,

  /// Media type for the baseline render.
  ///
  /// `screen` uses a viewport screenshot (like typical web rendering), while `print` uses Chrome's
  /// print-to-PDF pipeline and converts the resulting PDF into a stacked PNG so pagination can be
  /// diffed against FastRender.
  #[arg(long, value_enum, default_value_t = MediaMode::Screen)]
  media: MediaMode,

  /// Per-fixture hard timeout in seconds (0 = no timeout).
  #[arg(long, default_value_t = 15, value_name = "SECS")]
  timeout: u64,

  /// Enable or disable JavaScript (default: off).
  #[arg(long, value_enum, default_value_t = JsMode::Off)]
  js: JsMode,

  /// Allow CSS animations/transitions in the Chrome baseline (default: disabled for determinism).
  ///
  /// FastRender does not run CSS animations, so leaving them enabled in Chrome produces baselines
  /// that are both less deterministic (frame timing variance) and less aligned with renderer
  /// semantics.
  #[arg(long)]
  allow_animations: bool,

  /// Allow dark-mode / prefers-color-scheme defaults (do not force light color-scheme + white background).
  #[arg(long)]
  allow_dark_mode: bool,
}

#[derive(Copy, Clone, Debug, Eq, PartialEq, ValueEnum)]
enum JsMode {
  On,
  Off,
}

#[derive(Copy, Clone, Debug, Eq, PartialEq, ValueEnum)]
#[clap(rename_all = "lowercase")]
enum MediaMode {
  Screen,
  Print,
}

impl MediaMode {
  fn as_str(self) -> &'static str {
    match self {
      Self::Screen => "screen",
      Self::Print => "print",
    }
  }
}

#[derive(Debug, Clone)]
struct Fixture {
  stem: String,
  dir: PathBuf,
}

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
enum HeadlessMode {
  New,
  Legacy,
}

#[derive(Debug, Serialize)]
struct FixtureMetadata {
  fixture: String,
  fixture_dir: PathBuf,
  viewport: (u32, u32),
  dpr: f32,
  #[serde(skip_serializing_if = "Option::is_none")]
  chrome_window: Option<(u32, u32)>,
  #[serde(skip_serializing_if = "Option::is_none")]
  chrome_window_padding_css: Option<u32>,
  media: &'static str,
  js: JsModeMetadata,
  input_sha256: String,
  #[serde(skip_serializing_if = "Option::is_none")]
  assets_sha256: Option<String>,
  #[serde(skip_serializing_if = "Option::is_none")]
  shared_assets_sha256: Option<String>,
  headless: &'static str,
  chrome_version: Option<String>,
  elapsed_ms: f64,
}

#[derive(Copy, Clone, Debug, Serialize)]
#[serde(rename_all = "lowercase")]
enum JsModeMetadata {
  On,
  Off,
}

pub fn run_chrome_baseline_fixtures(args: ChromeBaselineFixturesArgs) -> Result<()> {
  if args.dpr <= 0.0 || !args.dpr.is_finite() {
    bail!("--dpr must be a positive, finite number");
  }

  let repo_root = crate::repo_root();
  let fixture_root = absolutize_path(&repo_root, &args.fixture_dir);
  let out_dir = absolutize_path(&repo_root, &args.out_dir);
  fs::create_dir_all(&out_dir)
    .with_context(|| format!("create output dir {}", out_dir.display()))?;

  if !fixture_root.is_dir() {
    bail!("fixture dir not found: {}", fixture_root.display());
  }

  let shared_assets_sha256 = compute_shared_assets_sha256(&fixture_root)?;

  let chrome =
    resolve_chrome_binary(&args).with_context(|| "failed to locate a Chrome/Chromium binary")?;
  let chrome_version = chrome_version(&chrome).ok();

  let temp_root = create_temp_root(&chrome)?;

  let fixtures = discover_fixtures(&fixture_root)?;
  let requested = if let Some(list) = args.fixtures.as_deref() {
    Some(list)
  } else if !args.fixtures_pos.is_empty() {
    Some(args.fixtures_pos.as_slice())
  } else {
    None
  };
  let fixtures = select_fixtures(fixtures, requested, args.shard)?;

  println!("Chrome: {}", chrome.display());
  println!("Input:  {}", fixture_root.display());
  println!("Output: {}", out_dir.display());
  println!(
    "Viewport: {}x{}  DPR: {}  Media: {}  JS: {}  Animations: {}  Timeout: {}s",
    args.viewport.0,
    args.viewport.1,
    args.dpr,
    args.media.as_str(),
    match args.js {
      JsMode::On => "on",
      JsMode::Off => "off",
    },
    if args.allow_animations { "on" } else { "off" },
    args.timeout
  );
  println!();

  let timeout = if args.timeout == 0 {
    None
  } else {
    Some(Duration::from_secs(args.timeout))
  };

  let mut ok = 0usize;
  let mut fail = 0usize;

  for fixture in fixtures {
    match render_fixture(
      &fixture,
      &chrome,
      chrome_version.as_deref(),
      shared_assets_sha256.as_deref(),
      &out_dir,
      temp_root.path(),
      &args,
      timeout,
    ) {
      Ok(()) => {
        ok += 1;
        println!("✓ {}", fixture.stem);
      }
      Err(err) => {
        fail += 1;
        eprintln!("✗ {}: {err:#}", fixture.stem);
      }
    }
  }

  println!();
  println!("Done: {ok} ok, {fail} failed");
  if fail > 0 {
    bail!(
      "{fail} fixture(s) failed (see logs under {})",
      out_dir.display()
    );
  }
  Ok(())
}

fn render_fixture(
  fixture: &Fixture,
  chrome: &Path,
  chrome_version: Option<&str>,
  shared_assets_sha256: Option<&str>,
  out_dir: &Path,
  temp_root: &Path,
  args: &ChromeBaselineFixturesArgs,
  timeout: Option<Duration>,
) -> Result<()> {
  let index_path = fixture.dir.join("index.html");
  if !index_path.is_file() {
    bail!("missing index.html: {}", index_path.display());
  }
  let index_bytes =
    fs::read(&index_path).with_context(|| format!("read {}", index_path.display()))?;

  let output_png = out_dir.join(format!("{}.png", fixture.stem));
  let chrome_log = out_dir.join(format!("{}.chrome.log", fixture.stem));
  let metadata_path = out_dir.join(format!("{}.json", fixture.stem));

  // Avoid leaving stale output artifacts around when a fixture fails to render (for example when
  // Chrome times out or crashes). We treat each run as authoritative; if it fails, callers should
  // not accidentally reuse a PNG/metadata from an earlier successful run.
  for path in [&output_png, &chrome_log, &metadata_path] {
    if path.exists() {
      let _ = fs::remove_file(path);
    }
  }

  let profile_dir = temp_root.join("profile").join(&fixture.stem);
  fs::create_dir_all(&profile_dir)
    .with_context(|| format!("create chrome profile dir {}", profile_dir.display()))?;
  if matches!(args.js, JsMode::Off) {
    write_js_disabled_chrome_preferences(&profile_dir)?;
  }

  let base_url = Url::from_directory_path(&fixture.dir)
    .map(|u| u.to_string())
    .map_err(|_| {
      anyhow!(
        "could not convert {} to a file:// base URL",
        fixture.dir.display()
      )
    })?;
  let patched_dir = temp_root.join("html");
  fs::create_dir_all(&patched_dir).context("create patched HTML directory")?;
  let patched_html = patched_dir.join(format!("{}.html", fixture.stem));
  let patched = fixture_html_patch::patch_html_bytes(
    &index_bytes,
    Some(&base_url),
    matches!(args.js, JsMode::Off),
    !args.allow_animations,
    args.allow_dark_mode,
  );
  fs::write(&patched_html, patched).with_context(|| format!("write {}", patched_html.display()))?;
  let url = file_url(&patched_html)?;

  let chrome_window_meta: Option<((u32, u32), u32)> = match args.media {
    MediaMode::Screen => {
      let pad_px = headless_window_viewport_height_pad_px()?;
      Some((
        (args.viewport.0, args.viewport.1.saturating_add(pad_px)),
        pad_px,
      ))
    }
    MediaMode::Print => None,
  };

  let start = Instant::now();
  let headless_used = match args.media {
    MediaMode::Screen => {
      let tmp_png_dir = temp_root.join("screenshots");
      fs::create_dir_all(&tmp_png_dir).context("create temp screenshot directory")?;
      let tmp_png_path = tmp_png_dir.join(format!("{}.png", fixture.stem));

      if tmp_png_path.exists() {
        let _ = fs::remove_file(&tmp_png_path);
      }

      let headless_used = run_chrome_screenshot(
        chrome,
        &url,
        &profile_dir,
        args.viewport,
        args.dpr,
        &tmp_png_path,
        &chrome_log,
        timeout,
        matches!(args.js, JsMode::Off),
      )?;

      let screenshot_len = fs::metadata(&tmp_png_path)
        .with_context(|| {
          format!(
            "chrome did not produce a screenshot for {} (see {})",
            fixture.stem,
            chrome_log.display()
          )
        })?
        .len();
      if screenshot_len == 0 {
        bail!(
          "chrome produced an empty screenshot for {} (see {})",
          fixture.stem,
          chrome_log.display()
        );
      }

      crop_chrome_screenshot(&tmp_png_path, &output_png, args.viewport, args.dpr)?;

      headless_used
    }
    MediaMode::Print => {
      let print_dir = temp_root.join("print").join(&fixture.stem);
      if print_dir.exists() {
        let _ = fs::remove_dir_all(&print_dir);
      }
      fs::create_dir_all(&print_dir)
        .with_context(|| format!("create print scratch dir {}", print_dir.display()))?;

      let tmp_pdf_path = print_dir.join(format!("{}.pdf", fixture.stem));
      if tmp_pdf_path.exists() {
        let _ = fs::remove_file(&tmp_pdf_path);
      }

      let headless_used = run_chrome_print_to_pdf(
        chrome,
        &url,
        &profile_dir,
        args.viewport,
        args.dpr,
        &tmp_pdf_path,
        &chrome_log,
        timeout,
      )?;

      convert_pdf_to_stacked_png(
        &tmp_pdf_path,
        &output_png,
        args.dpr,
        &print_dir,
        &chrome_log,
      )?;

      headless_used
    }
  };

  let elapsed_ms = start.elapsed().as_secs_f64() * 1000.0;

  let metadata = build_fixture_metadata(
    fixture,
    args,
    chrome_window_meta,
    headless_used,
    chrome_version,
    elapsed_ms,
    &index_bytes,
    shared_assets_sha256,
  )?;
  let json = serde_json::to_vec_pretty(&metadata).context("serialize chrome fixture metadata")?;
  fs::write(&metadata_path, json).with_context(|| format!("write {}", metadata_path.display()))?;

  Ok(())
}

fn build_fixture_metadata(
  fixture: &Fixture,
  args: &ChromeBaselineFixturesArgs,
  chrome_window: Option<((u32, u32), u32)>,
  headless_used: HeadlessMode,
  chrome_version: Option<&str>,
  elapsed_ms: f64,
  index_html: &[u8],
  shared_assets_sha256: Option<&str>,
) -> Result<FixtureMetadata> {
  let input_sha256 = sha256_hex(index_html);
  let assets_sha256 = compute_assets_sha256(&fixture.dir)?;

  Ok(FixtureMetadata {
    fixture: fixture.stem.clone(),
    fixture_dir: fixture.dir.clone(),
    viewport: args.viewport,
    dpr: args.dpr,
    chrome_window: chrome_window.map(|(window, _)| window),
    chrome_window_padding_css: chrome_window.map(|(_, padding)| padding).filter(|v| *v > 0),
    media: args.media.as_str(),
    js: match args.js {
      JsMode::On => JsModeMetadata::On,
      JsMode::Off => JsModeMetadata::Off,
    },
    input_sha256,
    assets_sha256,
    shared_assets_sha256: shared_assets_sha256.map(|hash| hash.to_string()),
    headless: match headless_used {
      HeadlessMode::New => "new",
      HeadlessMode::Legacy => "legacy",
    },
    chrome_version: chrome_version.map(|v| v.to_string()),
    elapsed_ms,
  })
}

fn sha256_hex(bytes: &[u8]) -> String {
  let digest = Sha256::digest(bytes);
  digest.iter().map(|b| format!("{b:02x}")).collect()
}

fn css_to_px(css_px: u32, dpr: f32) -> u32 {
  ((css_px as f32) * dpr).round().max(0.0) as u32
}

fn crop_chrome_screenshot(
  input_png: &Path,
  output_png: &Path,
  viewport: (u32, u32),
  dpr: f32,
) -> Result<()> {
  let image = image::open(input_png).with_context(|| format!("decode {}", input_png.display()))?;
  let expected_width = css_to_px(viewport.0, dpr);
  let expected_height = css_to_px(viewport.1, dpr);

  if image.width() < expected_width || image.height() < expected_height {
    bail!(
      "chrome screenshot is smaller than expected viewport: got {}x{}, expected at least {}x{} (viewport {}x{} @ dpr {})",
      image.width(),
      image.height(),
      expected_width,
      expected_height,
      viewport.0,
      viewport.1,
      dpr
    );
  }

  let cropped = image.crop_imm(0, 0, expected_width, expected_height);
  cropped
    .save_with_format(output_png, image::ImageFormat::Png)
    .with_context(|| format!("write {}", output_png.display()))?;
  Ok(())
}

fn compute_assets_sha256(fixture_dir: &Path) -> Result<Option<String>> {
  let assets_dir = fixture_dir.join("assets");
  let mut hasher = Sha256::new();
  let mut any = false;

  // Hash `assets/**` using paths relative to the assets directory. This preserves compatibility with
  // older `assets_sha256` values that only covered the `assets/` subtree.
  if assets_dir.is_dir() {
    let mut files = WalkDir::new(&assets_dir)
      .follow_links(false)
      .into_iter()
      .filter_map(|entry| entry.ok())
      .filter(|entry| entry.file_type().is_file())
      .map(|entry| entry.into_path())
      .collect::<Vec<_>>();

    files.sort_by(|a, b| {
      let a_rel = a.strip_prefix(&assets_dir).unwrap_or(a);
      let b_rel = b.strip_prefix(&assets_dir).unwrap_or(b);
      a_rel.to_string_lossy().cmp(&b_rel.to_string_lossy())
    });

    // Preserve the previous behavior where the mere presence of an `assets/` directory caused an
    // `assets_sha256` value to be emitted (even if it was empty).
    any = true;
    for path in files {
      let rel = path.strip_prefix(&assets_dir).unwrap_or(&path);
      hasher.update(rel.to_string_lossy().as_bytes());
      hasher.update([0u8]);
      let bytes = fs::read(&path).with_context(|| format!("read {}", path.display()))?;
      hasher.update(bytes);
      hasher.update([0u8]);
    }
  }

  // Some fixtures keep additional inputs (e.g. `styles.css`, `mask.svg`) at the fixture root.
  // Include those in the fingerprint too so `--no-chrome` can detect drift even when `index.html`
  // didn't change.
  let mut extra_files = WalkDir::new(fixture_dir)
    .follow_links(false)
    .into_iter()
    .filter_map(|entry| entry.ok())
    .filter(|entry| entry.file_type().is_file())
    .map(|entry| entry.into_path())
    .filter(|path| {
      if *path == fixture_dir.join("index.html") {
        return false;
      }
      if assets_dir.is_dir() && path.starts_with(&assets_dir) {
        return false;
      }
      true
    })
    .collect::<Vec<_>>();

  extra_files.sort_by(|a, b| {
    let a_rel = a.strip_prefix(fixture_dir).unwrap_or(a);
    let b_rel = b.strip_prefix(fixture_dir).unwrap_or(b);
    a_rel.to_string_lossy().cmp(&b_rel.to_string_lossy())
  });

  if !extra_files.is_empty() {
    if any {
      hasher.update(b"\0EXTRA\0");
    }
    any = true;
    for path in extra_files {
      let rel = path.strip_prefix(fixture_dir).unwrap_or(&path);
      hasher.update(rel.to_string_lossy().as_bytes());
      hasher.update([0u8]);
      let bytes = fs::read(&path).with_context(|| format!("read {}", path.display()))?;
      hasher.update(bytes);
      hasher.update([0u8]);
    }
  }

  if !any {
    return Ok(None);
  }

  let digest = hasher.finalize();
  Ok(Some(digest.iter().map(|b| format!("{b:02x}")).collect()))
}

fn compute_shared_assets_sha256(fixtures_root: &Path) -> Result<Option<String>> {
  let assets_dir = fixtures_root.join("assets");
  if !assets_dir.is_dir() {
    return Ok(None);
  }

  let mut files = WalkDir::new(&assets_dir)
    .follow_links(false)
    .into_iter()
    .filter_map(|entry| entry.ok())
    .filter(|entry| entry.file_type().is_file())
    .map(|entry| entry.into_path())
    .collect::<Vec<_>>();

  files.sort_by(|a, b| {
    let a_rel = a.strip_prefix(&assets_dir).unwrap_or(a);
    let b_rel = b.strip_prefix(&assets_dir).unwrap_or(b);
    a_rel.to_string_lossy().cmp(&b_rel.to_string_lossy())
  });

  let mut hasher = Sha256::new();
  for path in files {
    let rel = path.strip_prefix(&assets_dir).unwrap_or(&path);
    hasher.update(rel.to_string_lossy().as_bytes());
    hasher.update([0u8]);
    let bytes = fs::read(&path).with_context(|| format!("read {}", path.display()))?;
    hasher.update(bytes);
    hasher.update([0u8]);
  }

  let digest = hasher.finalize();
  Ok(Some(digest.iter().map(|b| format!("{b:02x}")).collect()))
}

fn convert_pdf_to_stacked_png(
  pdf_path: &Path,
  output_png: &Path,
  dpr: f32,
  scratch_dir: &Path,
  log_path: &Path,
) -> Result<()> {
  if !pdf_path.is_file() {
    bail!("missing PDF output at {}", pdf_path.display());
  }

  let pdftoppm = find_in_path("pdftoppm").ok_or_else(|| {
    anyhow!(
      "pdftoppm not found in PATH.\n\
       Install poppler-utils (Ubuntu: `sudo apt-get install poppler-utils`) to enable --media print baselines."
    )
  })?;

  let dpi = (96.0 * dpr).round().max(1.0) as u32;
  let prefix = scratch_dir.join("pages");
  let prefix_str = prefix.to_string_lossy();

  let output = Command::new(&pdftoppm)
    .args(["-png", "-r"])
    .arg(dpi.to_string())
    .arg(pdf_path)
    .arg(prefix_str.as_ref())
    .output()
    .with_context(|| format!("run {}", pdftoppm.display()))?;

  if let Some(parent) = log_path.parent() {
    let _ = fs::create_dir_all(parent);
  }
  let mut log = OpenOptions::new()
    .create(true)
    .append(true)
    .open(log_path)
    .with_context(|| format!("open log file {}", log_path.display()))?;
  writeln!(
    log,
    "\n\n# pdf->png: {} -png -r {} {} {}",
    pdftoppm.display(),
    dpi,
    pdf_path.display(),
    prefix.display()
  )
  .ok();
  if !output.stdout.is_empty() {
    let _ = log.write_all(&output.stdout);
  }
  if !output.stderr.is_empty() {
    let _ = log.write_all(&output.stderr);
  }

  if !output.status.success() {
    bail!("pdftoppm failed with status {}", output.status);
  }

  let mut pages: Vec<PathBuf> = fs::read_dir(scratch_dir)
    .with_context(|| format!("read {}", scratch_dir.display()))?
    .filter_map(|entry| entry.ok())
    .map(|entry| entry.path())
    .filter(|path| {
      path.extension().is_some_and(|ext| ext == "png")
        && path
          .file_name()
          .and_then(|name| name.to_str())
          .is_some_and(|name| name.starts_with("pages-"))
    })
    .collect();
  pages.sort_by_key(|path| {
    path
      .file_stem()
      .and_then(|stem| stem.to_str())
      .and_then(|stem| stem.rsplit('-').next())
      .and_then(|n| n.parse::<u32>().ok())
      .unwrap_or(u32::MAX)
  });
  if pages.is_empty() {
    bail!(
      "pdftoppm produced no PNG pages for {} (see {})",
      pdf_path.display(),
      log_path.display()
    );
  }

  let mut decoded = Vec::with_capacity(pages.len());
  let mut total_width: u32 = 0;
  let mut total_height: u32 = 0;
  for path in &pages {
    let image = image::open(path).with_context(|| format!("decode {}", path.display()))?;
    let rgba = image.to_rgba8();
    total_width = total_width.max(rgba.width());
    total_height = total_height.saturating_add(rgba.height());
    decoded.push(rgba);
  }

  let mut stacked: ImageBuffer<Rgba<u8>, Vec<u8>> =
    ImageBuffer::from_pixel(total_width, total_height, Rgba([255, 255, 255, 255]));
  let mut y: i64 = 0;
  for page in decoded {
    stacked
      .copy_from(&page, 0, y as u32)
      .context("stack PDF pages")?;
    y += page.height() as i64;
  }

  image::DynamicImage::ImageRgba8(stacked)
    .save_with_format(output_png, image::ImageFormat::Png)
    .with_context(|| format!("write {}", output_png.display()))?;

  Ok(())
}

fn run_chrome_print_to_pdf(
  chrome: &Path,
  url: &str,
  profile_dir: &Path,
  viewport: (u32, u32),
  dpr: f32,
  pdf_path: &Path,
  log_path: &Path,
  timeout: Option<Duration>,
) -> Result<HeadlessMode> {
  // Prefer `--headless=new`, but some Chrome builds (notably container/CI environments) will hang or
  // OOM in the new headless compositor for certain pages. Retrying with legacy headless keeps
  // baseline generation robust without changing behaviour when headless=new succeeds.
  let args_new = build_chrome_print_args(HeadlessMode::New, profile_dir, viewport, dpr, pdf_path)?;
  let mut last_status = run_chrome_with_timeout(chrome, &args_new, url, log_path, timeout, false);
  if last_status.as_ref().is_ok_and(|status| status.success()) && pdf_path.is_file() {
    return Ok(HeadlessMode::New);
  }

  let args_legacy =
    build_chrome_print_args(HeadlessMode::Legacy, profile_dir, viewport, dpr, pdf_path)?;
  if pdf_path.exists() {
    let _ = fs::remove_file(pdf_path);
  }
  let mut file = OpenOptions::new()
    .create(true)
    .append(true)
    .open(log_path)
    .with_context(|| format!("open log file {}", log_path.display()))?;
  writeln!(file, "\n\n# Retrying with --headless\n").ok();
  last_status = run_chrome_with_timeout(chrome, &args_legacy, url, log_path, timeout, true);
  let last_status = last_status?;
  if last_status.success() && pdf_path.is_file() {
    return Ok(HeadlessMode::Legacy);
  }

  if !last_status.success() {
    bail!(
      "chrome exited with {}; see {}",
      last_status,
      log_path.display()
    );
  }

  bail!("chrome did not produce a PDF; see {}", log_path.display());
}

fn build_chrome_print_args(
  headless: HeadlessMode,
  profile_dir: &Path,
  viewport: (u32, u32),
  dpr: f32,
  pdf_path: &Path,
) -> Result<Vec<String>> {
  let mut args = build_chrome_common_args(headless, profile_dir, viewport, dpr)?;
  args.push(format!("--print-to-pdf={}", pdf_path.display()));
  // Prefer the CSS @page header/footer content and keep PDF output deterministic.
  args.push("--print-to-pdf-no-header".to_string());
  Ok(args)
}

fn discover_fixtures(fixture_root: &Path) -> Result<Vec<Fixture>> {
  let mut fixtures = Vec::new();
  for entry in fs::read_dir(fixture_root)
    .with_context(|| format!("read fixture directory {}", fixture_root.display()))?
  {
    let entry = entry.context("read fixture dir entry")?;
    let file_type = entry.file_type().context("read fixture entry type")?;
    if !file_type.is_dir() {
      continue;
    }

    let stem = entry.file_name().to_string_lossy().to_string();
    let dir = entry.path();
    // The fixture root also contains shared support assets (e.g. `tests/pages/fixtures/assets`).
    // Only treat directories containing an `index.html` as renderable fixtures.
    if !dir.join("index.html").is_file() {
      continue;
    }
    fixtures.push(Fixture { stem, dir });
  }

  fixtures.sort_by(|a, b| a.stem.cmp(&b.stem));
  if fixtures.is_empty() {
    bail!(
      "no fixtures found under {} (expected <fixture>/index.html)",
      fixture_root.display()
    );
  }

  Ok(fixtures)
}

fn select_fixtures(
  mut fixtures: Vec<Fixture>,
  stems: Option<&[String]>,
  shard: Option<(usize, usize)>,
) -> Result<Vec<Fixture>> {
  if let Some(stems) = stems {
    let mut normalized = stems
      .iter()
      .map(|s| s.trim())
      .filter(|s| !s.is_empty())
      .collect::<Vec<_>>();
    normalized.sort();
    normalized.dedup();

    let want: HashSet<&str> = normalized.iter().copied().collect();
    let mut found = HashSet::<String>::new();
    fixtures.retain(|fixture| {
      if want.contains(fixture.stem.as_str()) {
        found.insert(fixture.stem.clone());
        true
      } else {
        false
      }
    });

    let mut missing = normalized
      .iter()
      .filter(|stem| !found.contains::<str>(*stem))
      .map(|stem| stem.to_string())
      .collect::<Vec<_>>();
    missing.sort();
    missing.dedup();
    if !missing.is_empty() {
      bail!("unknown fixture stem(s): {}", missing.join(", "));
    }
  }

  if let Some((index, total)) = shard {
    fixtures = fixtures
      .into_iter()
      .enumerate()
      .filter(|(i, _)| i % total == index)
      .map(|(_, fixture)| fixture)
      .collect();
  }

  if fixtures.is_empty() {
    bail!("no fixtures selected");
  }

  Ok(fixtures)
}

fn resolve_chrome_binary(args: &ChromeBaselineFixturesArgs) -> Result<PathBuf> {
  if let Some(chrome) = &args.chrome {
    let resolved = resolve_program_path(chrome)
      .with_context(|| format!("invalid --chrome {}", chrome.display()))?;
    return Ok(maybe_unwrap_snap_chromium(resolved));
  }

  if let Ok(value) = std::env::var("CHROME_BIN") {
    let trimmed = value.trim();
    if !trimmed.is_empty() {
      let resolved = resolve_program_path(Path::new(trimmed))
        .with_context(|| format!("invalid CHROME_BIN={trimmed}"))?;
      return Ok(maybe_unwrap_snap_chromium(resolved));
    }
  }

  const CANDIDATES: &[&str] = &[
    "google-chrome-stable",
    "google-chrome",
    "chromium",
    "chromium-browser",
    "chrome",
  ];

  if let Some(dir) = &args.chrome_dir {
    let dir = absolutize_path(&crate::repo_root(), dir);
    for name in CANDIDATES {
      let candidate = dir.join(name);
      if candidate.is_file() {
        return Ok(maybe_unwrap_snap_chromium(candidate));
      }
    }

    bail!(
      "No Chrome/Chromium binary found in {} (looked for {}).\n\
       Pass --chrome /path/to/chrome, set CHROME_BIN, or place a stub `chrome` binary in that directory.",
      dir.display(),
      CANDIDATES.join(", ")
    );
  }

  for name in CANDIDATES {
    if let Some(path) = find_in_path(name) {
      return Ok(maybe_unwrap_snap_chromium(path));
    }
  }

  bail!(
    "No Chrome/Chromium binary found.\n\
     Install one (e.g. google-chrome or chromium), pass --chrome /path/to/chrome, or set CHROME_BIN."
  );
}

fn maybe_unwrap_snap_chromium(chrome: PathBuf) -> PathBuf {
  // In some container environments the snap wrapper fails with systemd/DBus errors like:
  //
  //   cannot create transient scope: DBus error "org.freedesktop.DBus.Error.UnixProcessIdUnknown"
  //
  // The snap payload includes the real Chromium binary, which can be invoked directly without the
  // wrapper. Prefer that binary when the wrapper cannot even run `--version`.
  if !is_snap_chromium(&chrome) {
    return chrome;
  }
  if chrome_version(&chrome).is_ok() {
    return chrome;
  }

  for direct in [
    "/snap/chromium/current/usr/lib/chromium-browser/chrome",
    "/snap/chromium/current/usr/lib/chromium/chrome",
    "/snap/chromium/current/usr/lib/chromium-browser/chromium-browser",
    "/snap/chromium/current/usr/lib/chromium-browser/chromium",
    "/snap/chromium/current/usr/lib/chromium/chromium",
  ] {
    let direct = PathBuf::from(direct);
    if direct.is_file() {
      return direct;
    }
  }
  chrome
}

fn resolve_program_path(program: &Path) -> Result<PathBuf> {
  let has_separator = program.components().count() > 1;
  if has_separator || program.is_absolute() {
    if program.is_file() {
      return Ok(program.to_path_buf());
    }
    bail!("not found: {}", program.display());
  }

  let name = program
    .to_str()
    .ok_or_else(|| anyhow!("chrome program name is not valid UTF-8"))?;
  find_in_path(name).ok_or_else(|| anyhow!("not found in PATH: {}", program.display()))
}

fn find_in_path(program: &str) -> Option<PathBuf> {
  let path_var = std::env::var_os("PATH")?;
  for dir in std::env::split_paths(&path_var) {
    let candidate = dir.join(program);
    if candidate.is_file() {
      return Some(candidate);
    }
  }
  None
}

fn is_snap_chromium(chrome: &Path) -> bool {
  const SNAP_PREFIX: &str = "/snap/bin/chromium";
  if chrome.to_string_lossy().starts_with(SNAP_PREFIX) {
    return true;
  }

  // Some installations put a symlink/wrapper elsewhere (e.g. /usr/bin/chromium) that points into
  // the snap. If the canonicalized path resolves to the snap binary, treat it as snap Chromium.
  if let Ok(canon) = fs::canonicalize(chrome) {
    if canon.to_string_lossy().starts_with(SNAP_PREFIX) {
      return true;
    }
  }

  // Fall back to sniffing wrapper scripts. Avoid reading the full Chrome binary by only sampling
  // a small prefix.
  let mut file = match File::open(chrome) {
    Ok(f) => f,
    Err(_) => return false,
  };
  let mut buf = [0u8; 4096];
  let n = match file.read(&mut buf) {
    Ok(n) => n,
    Err(_) => return false,
  };
  let haystack = &buf[..n];
  for needle in [
    b"/snap/bin/chromium".as_slice(),
    b"snap run chromium".as_slice(),
    b"snap run chromium-browser".as_slice(),
  ] {
    if needle.len() <= haystack.len()
      && haystack
        .windows(needle.len())
        .any(|window| window == needle)
    {
      return true;
    }
  }
  false
}

fn create_temp_root(chrome: &Path) -> Result<TempDir> {
  // Snap-packaged Chromium is sandboxed from writing to arbitrary repo paths. Use a temp directory
  // under the snap's shared location when available so screenshot output is readable.
  let snap_common = if is_snap_chromium(chrome) {
    if let Ok(home) = std::env::var("HOME") {
      Some(PathBuf::from(home).join("snap/chromium/common"))
    } else {
      None
    }
  } else {
    None
  };

  if let Some(dir) = snap_common {
    let _ = fs::create_dir_all(&dir);
    if dir.is_dir() {
      return tempfile::Builder::new()
        .prefix("fastrender-chrome-fixtures.")
        .tempdir_in(&dir)
        .context("create snap temp dir for chrome fixtures");
    }
  }

  tempfile::Builder::new()
    .prefix("fastrender-chrome-fixtures.")
    .tempdir()
    .context("create temp dir for chrome fixtures")
}

fn chrome_version(chrome: &Path) -> Result<String> {
  let output = Command::new(chrome)
    .arg("--version")
    .output()
    .with_context(|| format!("run {} --version", chrome.display()))?;
  if !output.status.success() {
    bail!(
      "{} --version exited with {}",
      chrome.display(),
      output.status
    );
  }

  let stdout = String::from_utf8_lossy(&output.stdout);
  let stderr = String::from_utf8_lossy(&output.stderr);
  let version = stdout.trim();
  if !version.is_empty() {
    return Ok(version.to_string());
  }

  let version = stderr.trim();
  if !version.is_empty() {
    return Ok(version.to_string());
  }

  bail!("{} --version produced no output", chrome.display());
}

fn run_chrome_screenshot(
  chrome: &Path,
  url: &str,
  profile_dir: &Path,
  viewport: (u32, u32),
  dpr: f32,
  screenshot_path: &Path,
  log_path: &Path,
  timeout: Option<Duration>,
  disable_js: bool,
) -> Result<HeadlessMode> {
  let window_size = (
    viewport.0,
    viewport
      .1
      .saturating_add(headless_window_viewport_height_pad_px()?),
  );
  let virtual_time_budget_ms =
    disable_js.then_some(DEFAULT_HEADLESS_SCREENSHOT_VIRTUAL_TIME_BUDGET_MS);
  // Prefer `--headless=new`, but fall back to legacy headless when the new compositor fails.
  // In practice, `--headless=new` can hang or OOM in container/CI environments on complex pages.
  // Retrying keeps fixture diffs deterministic and avoids requiring callers to special-case pages.
  let args_new = build_chrome_args(
    HeadlessMode::New,
    profile_dir,
    window_size,
    dpr,
    screenshot_path,
    virtual_time_budget_ms,
  )?;
  let mut last_status = run_chrome_with_timeout(chrome, &args_new, url, log_path, timeout, false);
  if last_status.as_ref().is_ok_and(|status| status.success()) && screenshot_path.is_file() {
    // Some headless-new runs exit successfully yet fail to produce a real render (e.g. transient GPU
    // context failures in container environments). These produce mostly blank screenshots but still
    // satisfy the exit-status + "file exists" checks above, which makes fixture-vs-Chrome diffs
    // non-deterministic and unusable.
    //
    // If we detect a known compositor/GPU failure signature, treat the run as failed and retry
    // with legacy headless (`--headless --disable-gpu`), which is typically more robust.
    if !chrome_log_indicates_transient_gpu_failure(log_path) {
      return Ok(HeadlessMode::New);
    }
    let _ = fs::remove_file(screenshot_path);
    let mut file = OpenOptions::new()
      .create(true)
      .append(true)
      .open(log_path)
      .with_context(|| format!("open log file {}", log_path.display()))?;
    writeln!(
      file,
      "\n\n# Retrying with --headless (headless=new reported GPU compositor failure)\n"
    )
    .ok();
  }

  let args_legacy = build_chrome_args(
    HeadlessMode::Legacy,
    profile_dir,
    window_size,
    dpr,
    screenshot_path,
    virtual_time_budget_ms,
  )?;
  if screenshot_path.exists() {
    let _ = fs::remove_file(screenshot_path);
  }
  let mut file = OpenOptions::new()
    .create(true)
    .append(true)
    .open(log_path)
    .with_context(|| format!("open log file {}", log_path.display()))?;
  writeln!(file, "\n\n# Retrying with --headless\n").ok();
  last_status = run_chrome_with_timeout(chrome, &args_legacy, url, log_path, timeout, true);
  let last_status = last_status?;
  if last_status.success() && screenshot_path.is_file() {
    return Ok(HeadlessMode::Legacy);
  }

  if !last_status.success() {
    bail!(
      "chrome exited with {}; see {}",
      last_status,
      log_path.display()
    );
  }

  bail!(
    "chrome did not produce a screenshot; see {}",
    log_path.display()
  );
}

fn chrome_log_indicates_transient_gpu_failure(log_path: &Path) -> bool {
  let Ok(contents) = fs::read_to_string(log_path) else {
    return false;
  };
  // This error shows up in container/CI environments when Chrome's GPU process fails to initialize
  // a command buffer; the resulting `--screenshot` output is frequently blank even though Chrome
  // exits 0.
  //
  // Example:
  //   ContextResult::kTransientFailure: Failed to send GpuControl.CreateCommandBuffer.
  contents.contains("GpuControl.CreateCommandBuffer")
    || contents.contains("ContextResult::kTransientFailure")
}

fn build_chrome_args(
  headless: HeadlessMode,
  profile_dir: &Path,
  viewport: (u32, u32),
  dpr: f32,
  screenshot_path: &Path,
  virtual_time_budget_ms: Option<u64>,
) -> Result<Vec<String>> {
  let mut args = build_chrome_common_args(headless, profile_dir, viewport, dpr)?;
  if let Some(ms) = virtual_time_budget_ms {
    args.push(format!("--virtual-time-budget={ms}"));
  }
  args.push(format!("--screenshot={}", screenshot_path.display()));
  Ok(args)
}

fn build_chrome_common_args(
  headless: HeadlessMode,
  profile_dir: &Path,
  window_size: (u32, u32),
  dpr: f32,
) -> Result<Vec<String>> {
  let headless_flag = match headless {
    HeadlessMode::New => "--headless=new",
    HeadlessMode::Legacy => "--headless",
  };

  let viewport_arg = format!("--window-size={},{}", window_size.0, window_size.1);
  let dpr_arg = format!("--force-device-scale-factor={}", dpr);
  let profile_arg = format!("--user-data-dir={}", profile_dir.display());

  let mut args = vec![headless_flag.to_string()];
  // `--headless=new` uses Chrome's normal compositor pipeline, so disabling the GPU can break
  // screenshots on some builds (hangs/crashes before emitting `--screenshot` output). Legacy headless
  // mode is fine with `--disable-gpu` and historically recommended it for stability.
  if matches!(headless, HeadlessMode::Legacy) {
    args.push("--disable-gpu".to_string());
  }
  args.extend(
    [
      "--no-sandbox",
      "--disable-dev-shm-usage",
      "--hide-scrollbars",
      // Reduce nondeterminism + Chrome-vs-FastRender diffs caused by LCD subpixel text.
      //
      // Chrome's default text output often uses per-channel subpixel antialiasing (RGB fringes),
      // which FastRender does not currently emulate. Disabling LCD text keeps baseline renders
      // grayscale, improving diff signal for layout/paint primitives.
      "--disable-lcd-text",
      // When LCD text is disabled, subpixel positioning can still introduce 1px jitter across
      // platforms/builds. Disable it so text pixel placement is more stable.
      "--disable-font-subpixel-positioning",
    ]
    .iter()
    .map(|v| v.to_string()),
  );
  args.push(viewport_arg);
  args.push(dpr_arg);
  args.extend(
    [
      // Keep behaviour consistent with scripts/chrome_baseline.sh when loading local fixtures.
      "--disable-web-security",
      "--allow-file-access-from-files",
      // Keep renders deterministic/offline when fixture HTML accidentally references http(s).
      "--disable-background-networking",
      "--dns-prefetch-disable",
      "--no-first-run",
      "--no-default-browser-check",
      "--disable-component-update",
      "--disable-default-apps",
      "--disable-sync",
      "--host-resolver-rules=MAP * ~NOTFOUND, EXCLUDE localhost",
    ]
    .iter()
    .map(|v| v.to_string()),
  );
  args.push(profile_arg);
  Ok(args)
}

fn run_chrome_with_timeout(
  chrome: &Path,
  args: &[String],
  url: &str,
  log_path: &Path,
  timeout: Option<Duration>,
  append: bool,
) -> Result<ExitStatus> {
  if let Some(parent) = log_path.parent() {
    fs::create_dir_all(parent)
      .with_context(|| format!("create log directory {}", parent.display()))?;
  }

  let mut options = OpenOptions::new();
  options.create(true).write(true);
  if append {
    options.append(true);
  } else {
    options.truncate(true);
  }
  let mut log_file = options
    .open(log_path)
    .with_context(|| format!("open log file {}", log_path.display()))?;
  // Persist the command line used so logs are actionable even when Chrome itself produces no output.
  writeln!(log_file, "# chrome: {}", chrome.display()).ok();
  writeln!(log_file, "# url: {url}").ok();
  writeln!(log_file, "# args: {}", args.join(" ")).ok();
  writeln!(log_file).ok();

  #[cfg(target_os = "linux")]
  let rlimit_guard = {
    let min_gib = CHROME_MIN_RLIMIT_AS_BYTES / (1024 * 1024 * 1024);
    let guard = ChromeRlimitAsGuard::ensure_min_bytes(CHROME_MIN_RLIMIT_AS_BYTES).with_context(
      || {
        format!(
          "failed to raise RLIMIT_AS for Chrome (required >= {min_gib} GiB); try setting FASTR_XTASK_LIMIT_AS={min_gib}G or LIMIT_AS={min_gib}G (or 'unlimited')",
        )
      },
    )?;
    if let Some(guard) = guard.as_ref() {
      let (cur, max) = guard.previous_bytes();
      writeln!(
        log_file,
        "# note: raised RLIMIT_AS for chrome spawn (previous cur={cur} max={max} bytes)"
      )
      .ok();
    }
    guard
  };
  let stderr = log_file
    .try_clone()
    .with_context(|| format!("clone log file handle for {}", log_path.display()))?;

  let mut cmd = Command::new(chrome);
  cmd.args(args).arg(url);
  // Some Chrome/Chromium builds (notably the snap-packaged Chromium used in CI-like containers)
  // will crash with SIGTRAP when TMPDIR points outside their expected temp locations.
  //
  // This commonly happens when callers set TMPDIR to work around rustc filling `/tmp` during
  // builds; ensure the spawned browser uses its default temp directory instead of inheriting the
  // override.
  cmd.env_remove("TMPDIR");
  cmd.env_remove("TMP");
  cmd.env_remove("TEMP");

  // `scripts/cargo_agent.sh` runs cargo commands under RLIMIT_AS (virtual memory). Chrome tends to
  // reserve large address-space ranges (V8/Oilpan), which can trip these limits even when the
  // machine has plenty of actual RAM. Clear the address-space limit for the spawned Chrome process
  // so fixture baselines don't spuriously crash under the agent wrapper.
  #[cfg(target_os = "linux")]
  unsafe {
    cmd.pre_exec(|| {
      let lim = libc::rlimit {
        rlim_cur: libc::RLIM_INFINITY,
        rlim_max: libc::RLIM_INFINITY,
      };
      // Best-effort: if this fails (e.g. no CAP_SYS_RESOURCE), let Chrome run with the inherited
      // limit and rely on its own crash logging.
      let _ = libc::setrlimit(libc::RLIMIT_AS, &lim);
      Ok(())
    });
  }
  cmd
    .stdout(Stdio::from(log_file))
    .stderr(Stdio::from(stderr));

  let mut child = cmd
    .spawn()
    .with_context(|| format!("failed to launch chrome at {}", chrome.display()))?;
  #[cfg(target_os = "linux")]
  drop(rlimit_guard);
  let pid = child.id();

  if let Some(timeout) = timeout {
    let start = Instant::now();
    loop {
      if let Some(status) = child.try_wait().context("poll chrome status")? {
        return Ok(status);
      }
      if start.elapsed() >= timeout {
        let _ = child.kill();
        let _ = child.wait();
        bail!(
          "chrome timed out after {}s (pid {}); see {}",
          timeout.as_secs(),
          pid,
          log_path.display()
        );
      }
      std::thread::sleep(Duration::from_millis(50));
    }
  }

  child.wait().context("wait for chrome")
}

fn file_url(path: &Path) -> Result<String> {
  let absolute = if path.is_absolute() {
    path.to_path_buf()
  } else {
    std::env::current_dir()
      .context("resolve current directory")?
      .join(path)
  };

  Url::from_file_path(&absolute)
    .map(|u| u.to_string())
    .map_err(|_| anyhow!("could not convert {} to a file:// URL", absolute.display()))
}

fn absolutize_path(repo_root: &Path, path: &Path) -> PathBuf {
  if path.is_absolute() {
    path.to_path_buf()
  } else {
    repo_root.join(path)
  }
}

#[cfg(test)]
mod tests {
  use super::{
    build_fixture_metadata, chrome_log_indicates_transient_gpu_failure, is_snap_chromium,
    ChromeBaselineFixturesArgs,
  };
  use serde_json::Value;
  use sha2::{Digest, Sha256};
  use std::fs;
  use std::path::{Path, PathBuf};
  use tempfile::tempdir;

  #[test]
  fn snap_detection_matches_direct_path() {
    assert!(is_snap_chromium(Path::new("/snap/bin/chromium")));
  }

  #[test]
  fn snap_detection_matches_wrapper_script() {
    let temp = tempdir().expect("tempdir");
    let script = temp.path().join("chromium");
    fs::write(&script, "#!/bin/sh\nexec /snap/bin/chromium \"$@\"\n").expect("write wrapper");
    assert!(is_snap_chromium(&script));
  }

  #[test]
  fn snap_detection_matches_snap_run_wrapper_script() {
    let temp = tempdir().expect("tempdir");
    let script = temp.path().join("chromium-browser");
    fs::write(&script, "#!/bin/sh\nexec snap run chromium \"$@\"\n").expect("write wrapper");
    assert!(is_snap_chromium(&script));
  }

  #[test]
  fn js_off_profile_preferences_disable_javascript_content_settings() {
    let temp = tempdir().expect("tempdir");
    super::write_js_disabled_chrome_preferences(temp.path()).expect("write chrome prefs");
    let prefs_path = temp.path().join("Default/Preferences");
    let raw = fs::read_to_string(&prefs_path).expect("read chrome prefs");
    let json: Value = serde_json::from_str(&raw).expect("parse chrome prefs json");
    assert_eq!(
      json["profile"]["managed_default_content_settings"]["javascript"],
      2
    );
    assert_eq!(
      json["profile"]["default_content_setting_values"]["javascript"],
      2
    );
  }

  #[test]
  fn fixture_metadata_records_input_sha256() {
    let temp = tempdir().expect("tempdir");
    let fixture_dir = temp.path().join("fixture");
    fs::create_dir_all(&fixture_dir).expect("create fixture dir");

    let html = b"<!doctype html><title>fixture</title>";
    fs::write(fixture_dir.join("index.html"), html).expect("write index.html");

    let fixture = super::Fixture {
      stem: "fixture".to_string(),
      dir: fixture_dir,
    };

    let args = ChromeBaselineFixturesArgs {
      fixture_dir: PathBuf::from("fixtures"),
      out_dir: PathBuf::from("out"),
      fixtures: None,
      fixtures_pos: Vec::new(),
      shard: None,
      chrome: None,
      chrome_dir: None,
      viewport: (1040, 1240),
      dpr: 1.0,
      media: super::MediaMode::Screen,
      timeout: 15,
      js: super::JsMode::Off,
      allow_animations: false,
      allow_dark_mode: false,
    };

    let metadata = build_fixture_metadata(
      &fixture,
      &args,
      None,
      super::HeadlessMode::New,
      Some("Chromium 123.0.0.0"),
      12.0,
      html,
      None,
    )
    .expect("build fixture metadata");

    let digest = Sha256::digest(html);
    let expected = digest
      .iter()
      .map(|b| format!("{b:02x}"))
      .collect::<String>();
    assert_eq!(metadata.input_sha256, expected);

    let json = serde_json::to_string(&metadata).expect("serialize metadata");
    assert!(
      json.contains("\"input_sha256\""),
      "metadata JSON should include input_sha256; got: {json}"
    );
  }

  #[test]
  fn chrome_gpu_failure_detection_matches_expected_signatures() {
    let temp = tempdir().expect("tempdir");
    let log = temp.path().join("chrome.log");
    fs::write(
      &log,
      "[123:456] ContextResult::kTransientFailure: Failed to send GpuControl.CreateCommandBuffer.\n",
    )
    .expect("write log");
    assert!(chrome_log_indicates_transient_gpu_failure(&log));
    fs::write(&log, "everything is fine\n").expect("write log");
    assert!(!chrome_log_indicates_transient_gpu_failure(&log));
  }

  #[test]
  fn chrome_args_include_virtual_time_budget_when_requested() {
    let temp = tempdir().expect("tempdir");
    let screenshot = temp.path().join("out.png");
    let args = super::build_chrome_args(
      super::HeadlessMode::New,
      temp.path(),
      (1040, 1240),
      1.0,
      &screenshot,
      Some(super::DEFAULT_HEADLESS_SCREENSHOT_VIRTUAL_TIME_BUDGET_MS),
    )
    .expect("build chrome args");
    assert!(
      args.iter().any(|arg| arg == "--virtual-time-budget=5000"),
      "expected --virtual-time-budget=5000 in args: {args:?}"
    );
  }

  #[test]
  fn chrome_args_disable_lcd_text() {
    let temp = tempdir().expect("tempdir");
    let args =
      super::build_chrome_common_args(super::HeadlessMode::New, temp.path(), (1040, 1240), 1.0)
        .expect("build chrome args");
    assert!(
      args.iter().any(|arg| arg == "--disable-lcd-text"),
      "expected --disable-lcd-text in args: {args:?}"
    );
    assert!(
      args
        .iter()
        .any(|arg| arg == "--disable-font-subpixel-positioning"),
      "expected --disable-font-subpixel-positioning in args: {args:?}"
    );
  }
}
