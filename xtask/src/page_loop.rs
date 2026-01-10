use anyhow::{bail, Context, Result};
use clap::{ArgAction, Args, ValueEnum};
use fastrender::pageset::{pageset_entries_with_collisions, PagesetFilter};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use xtask::page_loop_plan::{build_bins_command, InspectFragCommandArgs};

const DEFAULT_FIXTURES_DIR: &str = "tests/pages/fixtures";
const DEFAULT_OUT_BASE: &str = "target/page_loop";
const DEFAULT_VIEWPORT: &str = "1040x1240";
const DEFAULT_DPR: f32 = 1.0;
const DEFAULT_TIMEOUT_SECS: u64 = 60;
const DEFAULT_JOBS: usize = 1;

#[derive(Copy, Clone, Debug, Eq, PartialEq, ValueEnum)]
#[clap(rename_all = "lowercase")]
pub(crate) enum MediaMode {
  Screen,
  Print,
}

impl MediaMode {
  fn as_cli_value(self) -> &'static str {
    match self {
      Self::Screen => "screen",
      Self::Print => "print",
    }
  }
}

#[derive(Args, Debug)]
pub struct PageLoopArgs {
  /// Offline fixture stem under tests/pages/fixtures (must contain an index.html).
  #[arg(
    long,
    value_name = "STEM",
    conflicts_with_all = ["pageset", "from_progress"],
    required_unless_present_any = ["pageset", "from_progress"]
  )]
  pub fixture: Option<String>,

  /// Pageset page URL or stem (from `src/pageset.rs`) to render via its fixture directory.
  ///
  /// This is resolved to a collision-aware fixture name (cache stem) before running.
  #[arg(
    long,
    value_name = "URL_OR_STEM",
    conflicts_with_all = ["fixture", "from_progress"]
  )]
  pub pageset: Option<String>,

  /// Select exactly one fixture from pageset progress JSON files in this directory.
  ///
  /// The directory should contain `*.json` files like `progress/pages/<stem>.json`.
  #[arg(long, value_name = "DIR", conflicts_with_all = ["fixture", "pageset"])]
  pub from_progress: Option<PathBuf>,

  /// When selecting from `--from-progress`, choose the first page whose `status != ok`
  /// (deterministic stem order).
  #[arg(long, requires = "from_progress", conflicts_with_all = ["top_worst_accuracy", "top_slowest"])]
  pub only_failures: bool,

  /// When selecting from `--from-progress`, choose the Nth worst-accuracy ok page (1-based)
  /// by `accuracy.diff_percent` (tie-break perceptual desc, then stem asc).
  ///
  /// If no selection flag is provided, `page-loop` defaults to `--top-worst-accuracy 1`.
  #[arg(
    long,
    value_name = "N",
    requires = "from_progress",
    conflicts_with_all = ["only_failures", "top_slowest"]
  )]
  pub top_worst_accuracy: Option<usize>,

  /// When selecting from `--from-progress`, choose the Nth slowest page (1-based) by `total_ms`.
  #[arg(
    long,
    value_name = "N",
    requires = "from_progress",
    conflicts_with_all = ["only_failures", "top_worst_accuracy"]
  )]
  pub top_slowest: Option<usize>,

  /// When selecting from `--from-progress`, only consider pages whose `hotspot` matches this value
  /// (case-insensitive).
  #[arg(long, value_name = "NAME", requires = "from_progress")]
  pub hotspot: Option<String>,

  /// Viewport size as WxH (e.g. 1040x1240; forwarded to renderers).
  #[arg(long, value_parser = crate::parse_viewport, default_value = DEFAULT_VIEWPORT)]
  pub viewport: (u32, u32),

  /// Device pixel ratio for media queries/srcset (forwarded to renderers).
  #[arg(long, default_value_t = DEFAULT_DPR)]
  pub dpr: f32,

  /// Number of parallel fixture renders for the FastRender step (forwarded to `render_fixtures --jobs/-j`).
  ///
  /// Since `page-loop` renders a single fixture, the default is 1 to avoid initializing an
  /// oversized renderer pool.
  #[arg(long, short, default_value_t = DEFAULT_JOBS, value_name = "N")]
  pub jobs: usize,

  /// Per-fixture hard timeout in seconds (forwarded to both FastRender and Chrome steps).
  #[arg(long, default_value_t = DEFAULT_TIMEOUT_SECS, value_name = "SECS")]
  pub timeout: u64,

  /// Media type for evaluating media queries.
  #[arg(long, value_enum, default_value_t = MediaMode::Screen)]
  pub media: MediaMode,

  /// Root directory to write output artifacts into.
  ///
  /// Defaults to `target/page_loop/<fixture>`.
  #[arg(long, value_name = "DIR")]
  pub out_dir: Option<PathBuf>,

  /// Also write per-fixture snapshot pipeline dumps (forwarded to render_fixtures).
  #[arg(long)]
  pub write_snapshot: bool,

  /// Render a debug overlay PNG via inspect_frag.
  #[arg(long)]
  pub overlay: bool,

  /// Dump inspect_frag pipeline stage JSON files into `<out_dir>/inspect`.
  ///
  /// This writes `dom.json`, `composed_dom.json`, `styled.json`, `box_tree.json`,
  /// `fragment_tree.json`, and `display_list.json`.
  #[arg(long)]
  pub inspect_dump_json: bool,

  /// Restrict inspect_frag dumps/overlays to the first node matching this selector.
  #[arg(long, value_name = "SELECTOR")]
  pub inspect_filter_selector: Option<String>,

  /// Restrict inspect_frag dumps/overlays to the first node matching this id attribute.
  #[arg(long, value_name = "ID")]
  pub inspect_filter_id: Option<String>,

  /// Dump custom properties for the inspected subtree into `<out_dir>/inspect/custom_properties.json`.
  #[arg(long, requires = "inspect_dump_json")]
  pub inspect_dump_custom_properties: bool,

  /// Only include custom properties whose name starts with this prefix (repeatable).
  #[arg(
    long,
    value_name = "PREFIX",
    requires = "inspect_dump_custom_properties",
    allow_hyphen_values = true
  )]
  pub inspect_custom_property_prefix: Vec<String>,

  /// Maximum number of custom properties to dump (after filtering/sorting).
  #[arg(long, value_name = "N", requires = "inspect_dump_custom_properties")]
  pub inspect_custom_properties_limit: Option<usize>,

  /// Run a Chrome baseline render and produce a diff report (Chrome vs FastRender).
  #[arg(long, action = ArgAction::SetTrue, conflicts_with = "no_chrome")]
  pub chrome: bool,

  /// Skip the Chrome baseline + diff report step.
  #[arg(long, action = ArgAction::SetTrue, conflicts_with = "chrome")]
  pub no_chrome: bool,

  /// Print the computed plan (commands + output paths) without executing.
  #[arg(long)]
  pub dry_run: bool,

  /// Use Cargo's debug profile for the FastRender/diff steps (skip `--release`).
  ///
  /// This is substantially faster to compile for quick inspection loops, at the cost of slower
  /// runtime and less representative performance numbers.
  #[arg(long)]
  pub debug: bool,
}

#[derive(Debug, Clone)]
struct Layout {
  root: PathBuf,
  fixtures_dir: PathBuf,
  fixture_stem: String,
  fixture_html: PathBuf,

  fastrender_dir: PathBuf,
  fastrender_png: PathBuf,
  fastrender_metadata: PathBuf,
  fastrender_snapshot: PathBuf,

  overlay_dir: PathBuf,
  overlay_png: PathBuf,

  inspect_dir: PathBuf,

  chrome_dir: PathBuf,
  chrome_png: PathBuf,

  report_html: PathBuf,
  report_json: PathBuf,
}

impl Layout {
  fn new(repo_root: &Path, fixture_stem: &str, out_root: &Path) -> Self {
    let fixtures_dir = repo_root.join(DEFAULT_FIXTURES_DIR);
    let fixture_html = fixtures_dir.join(fixture_stem).join("index.html");

    let fastrender_dir = out_root.join("fastrender");
    let fastrender_png = fastrender_dir.join(format!("{fixture_stem}.png"));
    let fastrender_metadata = fastrender_dir.join(format!("{fixture_stem}.json"));
    let fastrender_snapshot = fastrender_dir
      .join(fixture_stem)
      .join("snapshot.json");

    let overlay_dir = out_root.join("overlay");
    let overlay_png = overlay_dir.join(format!("{fixture_stem}.png"));

    let inspect_dir = out_root.join("inspect");

    let chrome_dir = out_root.join("chrome");
    let chrome_png = chrome_dir.join(format!("{fixture_stem}.png"));

    let report_html = out_root.join("report.html");
    let report_json = out_root.join("report.json");

    Self {
      root: out_root.to_path_buf(),
      fixtures_dir,
      fixture_stem: fixture_stem.to_string(),
      fixture_html,
      fastrender_dir,
      fastrender_png,
      fastrender_metadata,
      fastrender_snapshot,
      overlay_dir,
      overlay_png,
      inspect_dir,
      chrome_dir,
      chrome_png,
      report_html,
      report_json,
    }
  }
}

pub fn run_page_loop(args: PageLoopArgs) -> Result<()> {
  let repo_root = crate::repo_root();
  let fixture_stem = resolve_fixture_stem(&repo_root, &args)?;
  validate_args(&args, &fixture_stem)?;
  let out_root = resolve_out_root(&repo_root, &args, &fixture_stem)?;
  let layout = Layout::new(&repo_root, &fixture_stem, &out_root);

  if !layout.fixture_html.is_file() {
    bail!(
      "fixture does not exist: {}\n\
       expected fixture HTML at: {}\n\
       hint: fixtures live under {DEFAULT_FIXTURES_DIR}/<stem>/index.html",
      layout.fixture_stem,
      layout.fixture_html.display()
    );
  }

  let run_chrome = args.chrome && !args.no_chrome;
  let needs_inspect_frag = args.overlay || args.inspect_dump_json;
  let mut bins_to_build = vec!["render_fixtures"];
  if needs_inspect_frag {
    bins_to_build.push("inspect_frag");
  }
  if run_chrome {
    bins_to_build.push("diff_renders");
  }
  let build_bins_cmd = build_bins_command(&repo_root, args.debug, &bins_to_build);

  let render_fixtures_cmd = xtask::page_loop_plan::build_render_fixtures_command(
    &repo_root,
    args.debug,
    &layout.fixtures_dir,
    &layout.fastrender_dir,
    &layout.fixture_stem,
    args.jobs,
    args.viewport,
    args.dpr,
    args.media.as_cli_value(),
    args.timeout,
    run_chrome,
    args.write_snapshot,
  );

  let inspect_frag_cmd = if needs_inspect_frag {
    Some(xtask::page_loop_plan::build_inspect_frag_command(
      &repo_root,
      args.debug,
      &InspectFragCommandArgs {
        fixture_html: layout.fixture_html.clone(),
        overlay_png: args.overlay.then(|| layout.overlay_png.clone()),
        dump_json_dir: args.inspect_dump_json.then(|| layout.inspect_dir.clone()),
        filter_selector: args.inspect_filter_selector.clone(),
        filter_id: args.inspect_filter_id.clone(),
        dump_custom_properties: args.inspect_dump_custom_properties,
        custom_property_prefix: args.inspect_custom_property_prefix.clone(),
        custom_properties_limit: args.inspect_custom_properties_limit,
        viewport: args.viewport,
        dpr: args.dpr,
        media: args.media.as_cli_value().to_string(),
        timeout: args.timeout,
      },
    ))
  } else {
    None
  };

  let chrome_cmd = if run_chrome {
    Some(build_chrome_baseline_command(&repo_root, &layout, &args)?)
  } else {
    None
  };

  let diff_renders_cmd = if run_chrome {
    Some(build_diff_renders_command(&repo_root, &layout, args.debug)?)
  } else {
    None
  };

  if args.dry_run {
    println!("page-loop plan:");
    println!("  fixture: {}", layout.fixture_stem);
    println!("  html: {}", layout.fixture_html.display());
    println!("  out_dir: {}", layout.root.display());
    println!("  fastrender_png: {}", layout.fastrender_png.display());
    println!("  fastrender_metadata: {}", layout.fastrender_metadata.display());
    println!("  jobs: {}", args.jobs);
    println!("  timeout: {}s", args.timeout);
    if args.write_snapshot {
      println!("  fastrender_snapshot: {}", layout.fastrender_snapshot.display());
    }
    if args.overlay {
      println!("  overlay_png: {}", layout.overlay_png.display());
    }
    if args.inspect_dump_json {
      println!("  inspect_dir: {}", layout.inspect_dir.display());
    }
    if run_chrome {
      println!("  chrome_png: {}", layout.chrome_png.display());
      println!("  report_html: {}", layout.report_html.display());
      println!("  report_json: {}", layout.report_json.display());
    }
    println!();

    crate::print_command(&build_bins_cmd);
    crate::print_command(&render_fixtures_cmd);
    if let Some(cmd) = inspect_frag_cmd.as_ref() {
      crate::print_command(cmd);
    }
    if let Some(cmd) = chrome_cmd.as_ref() {
      crate::print_command(cmd);
    }
    if let Some(cmd) = diff_renders_cmd.as_ref() {
      crate::print_command(cmd);
    }
    return Ok(());
  }

  fs::create_dir_all(&layout.root).with_context(|| {
    format!(
      "failed to create page-loop output directory {}",
      layout.root.display()
    )
  })?;

  clear_dir(&layout.fastrender_dir).context("clear FastRender output dir")?;
  if args.overlay {
    clear_dir(&layout.overlay_dir).context("clear overlay output dir")?;
  }
  if args.inspect_dump_json {
    clear_dir(&layout.inspect_dir).context("clear inspect output dir")?;
  }
  if run_chrome {
    clear_dir(&layout.chrome_dir).context("clear Chrome output dir")?;
    remove_file_if_exists(&layout.report_html).context("clear existing report.html")?;
    remove_file_if_exists(&layout.report_json).context("clear existing report.json")?;
  }

  println!("Building renderer binaries...");
  crate::run_command(build_bins_cmd).context("build renderer binaries failed")?;

  println!("Rendering fixture with FastRender...");
  crate::run_command(render_fixtures_cmd).context("render_fixtures failed")?;

  if let Some(cmd) = inspect_frag_cmd {
    match (args.overlay, args.inspect_dump_json) {
      (true, true) => println!("Running inspect_frag (overlay + JSON dumps)..."),
      (true, false) => println!("Rendering debug overlay..."),
      (false, true) => println!("Dumping inspect_frag JSON..."),
      (false, false) => {}
    }
    crate::run_command(cmd).context("inspect_frag failed")?;
  }

  if let Some(cmd) = chrome_cmd {
    println!("Rendering Chrome baseline...");
    crate::run_command(cmd).context("chrome-baseline-fixtures failed")?;
  }

  if let Some(cmd) = diff_renders_cmd {
    println!("Diffing renders (Chrome vs FastRender)...");
    run_diff_renders_allowing_differences(cmd, &layout)?;
    println!("Report written to {}", layout.report_html.display());
  }

  Ok(())
}

fn validate_args(args: &PageLoopArgs, fixture_stem: &str) -> Result<()> {
  if fixture_stem.trim().is_empty() {
    bail!("--fixture must not be empty");
  }
  if fixture_stem.contains('/') || fixture_stem.contains('\\') || fixture_stem.contains("..") {
    bail!(
      "invalid --fixture value {:?}; expected a single fixture stem (directory name) under {DEFAULT_FIXTURES_DIR}",
      fixture_stem
    );
  }
  if args.dpr <= 0.0 || !args.dpr.is_finite() {
    bail!("--dpr must be a positive, finite number");
  }
  if args.jobs == 0 {
    bail!("--jobs must be > 0");
  }
  if args.timeout == 0 {
    bail!("--timeout must be > 0");
  }
  if (args.inspect_filter_selector.is_some() || args.inspect_filter_id.is_some())
    && !(args.overlay || args.inspect_dump_json)
  {
    bail!("--inspect-filter-selector/--inspect-filter-id require --overlay and/or --inspect-dump-json");
  }
  Ok(())
}

fn resolve_out_root(repo_root: &Path, args: &PageLoopArgs, fixture_stem: &str) -> Result<PathBuf> {
  let out_dir = args
    .out_dir
    .clone()
    .unwrap_or_else(|| PathBuf::from(DEFAULT_OUT_BASE).join(fixture_stem));

  if out_dir.as_os_str().is_empty() {
    bail!(
      "refusing unsafe --out-dir: empty path\n\
       pass something like --out-dir target/page_loop/<fixture>"
    );
  }

  let out_dir = if out_dir.is_absolute() {
    out_dir
  } else {
    repo_root.join(out_dir)
  };

  // Refuse to write into the filesystem root. This is a cheap safety net against typos like
  // `--out-dir /` when we later clear subdirectories.
  if out_dir.parent().is_none() {
    bail!(
      "refusing unsafe --out-dir: {}\n\
       choose a non-root output directory (e.g. target/page_loop/<fixture>)",
      out_dir.display()
    );
  }

  Ok(out_dir)
}

fn resolve_fixture_stem(repo_root: &Path, args: &PageLoopArgs) -> Result<String> {
  if args.from_progress.is_some() {
    return resolve_fixture_stem_from_progress(repo_root, args);
  }
  if let Some(fixture) = args.fixture.as_deref() {
    return Ok(fixture.trim().to_string());
  }
  let pageset = args
    .pageset
    .as_deref()
    .ok_or_else(|| anyhow::anyhow!("missing --fixture or --pageset argument"))?;
  resolve_pageset_to_fixture_stem(pageset)
}

fn resolve_pageset_to_fixture_stem(raw: &str) -> Result<String> {
  let pageset = raw.trim();
  if pageset.is_empty() {
    bail!("--pageset must not be empty");
  }

  let filter = PagesetFilter::from_inputs(&[pageset.to_string()])
    .ok_or_else(|| anyhow::anyhow!("invalid pageset selector: {pageset:?}"))?;

  let (entries, _collisions) = pageset_entries_with_collisions();
  let selected: Vec<_> = entries
    .into_iter()
    .filter(|entry| filter.matches_entry(entry))
    .collect();

  let missing = filter.unmatched(&selected);
  if selected.is_empty() || !missing.is_empty() {
    let listed = if missing.is_empty() {
      pageset.to_string()
    } else {
      missing.join(", ")
    };
    bail!("unknown pageset page(s): {listed}");
  }

  if selected.len() > 1 {
    let mut options = selected
      .iter()
      .map(|entry| format!("{} ({})", entry.cache_stem, entry.url))
      .collect::<Vec<_>>();
    options.sort();
    bail!(
      "pageset selector {:?} matches multiple pages: {}\n\
       hint: pass a full URL or the collision-aware cache stem (e.g. example.com--deadbeef) to disambiguate",
      pageset,
      options.join(", ")
    );
  }

  Ok(selected[0].cache_stem.clone())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProgressSelectionMode {
  OnlyFailures,
  TopWorstAccuracy { rank: usize },
  TopSlowest { rank: usize },
}

fn resolve_fixture_stem_from_progress(repo_root: &Path, args: &PageLoopArgs) -> Result<String> {
  let Some(progress_dir) = args.from_progress.as_deref() else {
    bail!("internal error: resolve_fixture_stem_from_progress called without --from-progress");
  };

  let progress_dir = resolve_repo_path(repo_root, progress_dir);
  if !progress_dir.is_dir() {
    bail!(
      "progress directory does not exist: {}",
      progress_dir.display()
    );
  }

  let mode = if args.only_failures {
    ProgressSelectionMode::OnlyFailures
  } else if let Some(rank) = args.top_slowest {
    ProgressSelectionMode::TopSlowest { rank }
  } else {
    ProgressSelectionMode::TopWorstAccuracy {
      rank: args.top_worst_accuracy.unwrap_or(1),
    }
  };

  match mode {
    ProgressSelectionMode::OnlyFailures => {}
    ProgressSelectionMode::TopWorstAccuracy { rank } => {
      if rank == 0 {
        bail!("--top-worst-accuracy must be > 0");
      }
    }
    ProgressSelectionMode::TopSlowest { rank } => {
      if rank == 0 {
        bail!("--top-slowest must be > 0");
      }
    }
  }

  let hotspot = args.hotspot.as_deref().map(str::trim);
  if args.hotspot.is_some() && hotspot == Some("") {
    bail!("--hotspot must not be empty");
  }
  let hotspot = hotspot.filter(|s| !s.is_empty());

  let fixtures_root = repo_root.join(DEFAULT_FIXTURES_DIR);
  let mut pages =
    xtask::pageset_failure_fixtures::read_progress_pages(&progress_dir, &fixtures_root)?;

  if let Some(hotspot) = hotspot {
    // Match the pageset_progress convention: hotspot filters are case-insensitive.
    pages.retain(|p| {
      p.hotspot
        .as_deref()
        .is_some_and(|value| value.eq_ignore_ascii_case(hotspot))
    });
    if pages.is_empty() {
      bail!(
        "no progress pages matched --hotspot {hotspot:?} under {}",
        progress_dir.display()
      );
    }
  }

  println!(
    "Progress selection: discovered {} entr{} in {}",
    pages.len(),
    if pages.len() == 1 { "y" } else { "ies" },
    progress_dir.display()
  );
  if let Some(hotspot) = hotspot {
    println!("Progress selection: hotspot filter: {hotspot}");
  }

  let selected = match mode {
    ProgressSelectionMode::OnlyFailures => {
      let mut failing = pages
        .into_iter()
        .filter(|p| p.status != "ok")
        .collect::<Vec<_>>();
      if failing.is_empty() {
        bail!(
          "no failing pages (status != ok) found under {}",
          progress_dir.display()
        );
      }
      // Prefer pages that have offline fixtures.
      let any_fixture = failing.iter().any(|p| p.has_fixture);
      if any_fixture {
        failing.retain(|p| p.has_fixture);
      }

      // Deterministic order: the input `pages` list is stem-sorted, but retain the guarantee after
      // filtering by sorting again.
      failing.sort_by(|a, b| a.stem.cmp(&b.stem));
      failing[0].clone()
    }
    ProgressSelectionMode::TopWorstAccuracy { rank } => {
      let mut candidates = pages
        .into_iter()
        .filter(|p| p.status == "ok" && p.accuracy.is_some())
        .collect::<Vec<_>>();
      if candidates.is_empty() {
        bail!(
          "no ok pages with accuracy metrics found under {}.\n\
           hint: run `bash scripts/cargo_agent.sh xtask pageset --accuracy ...` or `bash scripts/cargo_agent.sh xtask refresh-progress-accuracy ...` to populate `accuracy.diff_percent`.",
          progress_dir.display()
        );
      }

      let any_fixture = candidates.iter().any(|p| p.has_fixture);
      if any_fixture {
        candidates.retain(|p| p.has_fixture);
      }

      candidates.sort_by(|a, b| {
        let a_acc = a.accuracy.expect("filtered to accuracy pages");
        let b_acc = b.accuracy.expect("filtered to accuracy pages");
        b_acc
          .diff_percent
          .total_cmp(&a_acc.diff_percent)
          .then_with(|| {
            b_acc
              .perceptual
              .unwrap_or(0.0)
              .total_cmp(&a_acc.perceptual.unwrap_or(0.0))
          })
          .then_with(|| a.stem.cmp(&b.stem))
      });

      if rank > candidates.len() {
        bail!(
          "--top-worst-accuracy {rank} is out of range (only {} eligible page(s) found under {})",
          candidates.len(),
          progress_dir.display()
        );
      }
      candidates[rank - 1].clone()
    }
    ProgressSelectionMode::TopSlowest { rank } => {
      let mut candidates = pages
        .into_iter()
        .filter(|p| p.total_ms.is_some())
        .collect::<Vec<_>>();
      if candidates.is_empty() {
        bail!(
          "no pages with total_ms timings found under {}",
          progress_dir.display()
        );
      }

      let any_fixture = candidates.iter().any(|p| p.has_fixture);
      if any_fixture {
        candidates.retain(|p| p.has_fixture);
      }

      candidates.sort_by(|a, b| {
        // Safe unwrap: filtered to total_ms pages.
        b.total_ms
          .unwrap_or(0.0)
          .total_cmp(&a.total_ms.unwrap_or(0.0))
          .then_with(|| a.stem.cmp(&b.stem))
      });

      if rank > candidates.len() {
        bail!(
          "--top-slowest {rank} is out of range (only {} eligible page(s) found under {})",
          candidates.len(),
          progress_dir.display()
        );
      }
      candidates[rank - 1].clone()
    }
  };
  println!("Progress selection: selected {}", selected.stem);
  if selected.has_fixture {
    return Ok(selected.stem);
  }

  bail!(
    "selected page '{}' does not have an offline fixture.\n\
     Expected: {}\n\
     Hint: run `bash scripts/cargo_agent.sh xtask import-page-fixture <bundle.tar> {}` or `bash scripts/cargo_agent.sh xtask recapture-page-fixtures ...` to create it.",
    selected.stem,
    selected.fixture_index_path.display(),
    selected.stem
  );
}

fn resolve_repo_path(repo_root: &Path, path: &Path) -> PathBuf {
  if path.is_absolute() {
    path.to_path_buf()
  } else {
    repo_root.join(path)
  }
}

fn build_chrome_baseline_command(repo_root: &Path, layout: &Layout, args: &PageLoopArgs) -> Result<Command> {
  let xtask = std::env::current_exe().context("resolve current xtask executable path")?;
  let mut cmd = crate::cmd::run_limited_xtask_command(repo_root);
  cmd.arg(xtask);
  cmd
    .arg("chrome-baseline-fixtures")
    .arg("--fixture-dir")
    .arg(&layout.fixtures_dir)
    .arg("--fixtures")
    .arg(&layout.fixture_stem)
    .arg("--out-dir")
    .arg(&layout.chrome_dir)
    .arg("--viewport")
    .arg(format!("{}x{}", args.viewport.0, args.viewport.1))
    .arg("--dpr")
    .arg(args.dpr.to_string())
    .arg("--timeout")
    .arg(args.timeout.to_string())
    .arg("--media")
    .arg(args.media.as_cli_value());
  cmd.current_dir(repo_root);
  Ok(cmd)
}

fn build_diff_renders_command(repo_root: &Path, layout: &Layout, debug: bool) -> Result<Command> {
  let diff_renders_exe = xtask::page_loop_plan::diff_renders_executable(repo_root, debug);
  let mut cmd = crate::cmd::run_limited_command_default(repo_root);
  cmd.arg(&diff_renders_exe);
  cmd.arg("--before").arg(&layout.chrome_png);
  cmd.arg("--after").arg(&layout.fastrender_png);
  cmd.arg("--html").arg(&layout.report_html);
  cmd.arg("--json").arg(&layout.report_json);
  cmd.args(["--tolerance", "0"]);
  cmd.args(["--max-diff-percent", "0"]);
  cmd
    .arg("--sort-by")
    .arg("percent");
  Ok(cmd)
}

fn run_diff_renders_allowing_differences(mut cmd: Command, layout: &Layout) -> Result<()> {
  crate::print_command(&cmd);
  let output = cmd
    .output()
    .with_context(|| format!("failed to run {:?}", cmd.get_program()))?;

  if !output.stdout.is_empty() {
    print!("{}", String::from_utf8_lossy(&output.stdout));
  }
  if !output.stderr.is_empty() {
    eprint!("{}", String::from_utf8_lossy(&output.stderr));
  }

  if output.status.success() {
    return Ok(());
  }

  if output.status.code() == Some(1) {
    let stderr = String::from_utf8_lossy(&output.stderr);
    if stderr.trim_start().starts_with("error:") {
      bail!("diff_renders failed (see output above)");
    }
    eprintln!(
      "diff_renders reported differences; report: {}",
      layout.report_html.display()
    );
    return Ok(());
  }

  bail!("diff_renders failed with status {}", output.status);
}

fn clear_dir(path: &Path) -> Result<()> {
  if path.exists() {
    fs::remove_dir_all(path).with_context(|| format!("remove {}", path.display()))?;
  }
  fs::create_dir_all(path).with_context(|| format!("create {}", path.display()))?;
  Ok(())
}

fn remove_file_if_exists(path: &Path) -> Result<()> {
  if path.exists() {
    fs::remove_file(path).with_context(|| format!("remove {}", path.display()))?;
  }
  Ok(())
}
