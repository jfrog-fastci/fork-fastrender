use anyhow::{bail, Context, Result};
use clap::{ArgAction, Args, ValueEnum};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

const DEFAULT_FIXTURES_DIR: &str = "tests/pages/fixtures";
const DEFAULT_OUT_BASE: &str = "target/page_loop";
const DEFAULT_VIEWPORT: &str = "1040x1240";
const DEFAULT_DPR: f32 = 1.0;

#[derive(Copy, Clone, Debug, Eq, PartialEq, ValueEnum)]
#[clap(rename_all = "lowercase")]
enum MediaMode {
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
  #[arg(long, value_name = "STEM")]
  pub fixture: String,

  /// Viewport size as WxH (e.g. 1040x1240; forwarded to renderers).
  #[arg(long, value_parser = crate::parse_viewport, default_value = DEFAULT_VIEWPORT)]
  pub viewport: (u32, u32),

  /// Device pixel ratio for media queries/srcset (forwarded to renderers).
  #[arg(long, default_value_t = DEFAULT_DPR)]
  pub dpr: f32,

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

  /// Run a Chrome baseline render and produce a diff report (Chrome vs FastRender).
  #[arg(long, action = ArgAction::SetTrue, conflicts_with = "no_chrome")]
  pub chrome: bool,

  /// Skip the Chrome baseline + diff report step.
  #[arg(long, action = ArgAction::SetTrue, conflicts_with = "chrome")]
  pub no_chrome: bool,

  /// Print the computed plan (commands + output paths) without executing.
  #[arg(long)]
  pub dry_run: bool,
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
      chrome_dir,
      chrome_png,
      report_html,
      report_json,
    }
  }
}

pub fn run_page_loop(args: PageLoopArgs) -> Result<()> {
  validate_args(&args)?;

  let repo_root = crate::repo_root();
  let out_root = resolve_out_root(&repo_root, &args)?;
  let layout = Layout::new(&repo_root, &args.fixture, &out_root);

  if !layout.fixture_html.is_file() {
    bail!(
      "fixture does not exist: {}\n\
       expected fixture HTML at: {}\n\
       hint: fixtures live under {DEFAULT_FIXTURES_DIR}/<stem>/index.html",
      args.fixture,
      layout.fixture_html.display()
    );
  }

  let render_fixtures_cmd = build_render_fixtures_command(&repo_root, &layout, &args)?;
  let overlay_cmd = if args.overlay {
    Some(build_inspect_frag_overlay_command(&repo_root, &layout, &args)?)
  } else {
    None
  };

  let run_chrome = args.chrome && !args.no_chrome;
  let chrome_cmd = if run_chrome {
    Some(build_chrome_baseline_command(&repo_root, &layout, &args)?)
  } else {
    None
  };

  let build_diff_renders_cmd = if run_chrome {
    Some(build_diff_renders_build_command(&repo_root))
  } else {
    None
  };
  let diff_renders_cmd = if run_chrome {
    Some(build_diff_renders_command(&repo_root, &layout)?)
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
    if args.write_snapshot {
      println!("  fastrender_snapshot: {}", layout.fastrender_snapshot.display());
    }
    if args.overlay {
      println!("  overlay_png: {}", layout.overlay_png.display());
    }
    if run_chrome {
      println!("  chrome_png: {}", layout.chrome_png.display());
      println!("  report_html: {}", layout.report_html.display());
      println!("  report_json: {}", layout.report_json.display());
    }
    println!();

    crate::print_command(&render_fixtures_cmd);
    if let Some(cmd) = overlay_cmd.as_ref() {
      crate::print_command(cmd);
    }
    if let Some(cmd) = chrome_cmd.as_ref() {
      crate::print_command(cmd);
    }
    if let Some(cmd) = build_diff_renders_cmd.as_ref() {
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
  if run_chrome {
    clear_dir(&layout.chrome_dir).context("clear Chrome output dir")?;
    remove_file_if_exists(&layout.report_html).context("clear existing report.html")?;
    remove_file_if_exists(&layout.report_json).context("clear existing report.json")?;
  }

  println!("Rendering fixture with FastRender...");
  crate::run_command(render_fixtures_cmd).context("render_fixtures failed")?;

  if let Some(cmd) = overlay_cmd {
    println!("Rendering debug overlay...");
    crate::run_command(cmd).context("inspect_frag overlay failed")?;
  }

  if let Some(cmd) = chrome_cmd {
    println!("Rendering Chrome baseline...");
    crate::run_command(cmd).context("chrome-baseline-fixtures failed")?;
  }

  if let Some(cmd) = build_diff_renders_cmd {
    println!("Building diff_renders...");
    crate::run_command(cmd).context("build diff_renders failed")?;
  }

  if let Some(cmd) = diff_renders_cmd {
    println!("Diffing renders (Chrome vs FastRender)...");
    run_diff_renders_allowing_differences(cmd, &layout)?;
    println!("Report written to {}", layout.report_html.display());
  }

  Ok(())
}

fn validate_args(args: &PageLoopArgs) -> Result<()> {
  if args.fixture.trim().is_empty() {
    bail!("--fixture must not be empty");
  }
  if args.fixture.contains('/') || args.fixture.contains('\\') || args.fixture.contains("..") {
    bail!(
      "invalid --fixture value {:?}; expected a single fixture stem (directory name) under {DEFAULT_FIXTURES_DIR}",
      args.fixture
    );
  }
  if args.dpr <= 0.0 || !args.dpr.is_finite() {
    bail!("--dpr must be a positive, finite number");
  }
  Ok(())
}

fn resolve_out_root(repo_root: &Path, args: &PageLoopArgs) -> Result<PathBuf> {
  let out_dir = args
    .out_dir
    .clone()
    .unwrap_or_else(|| PathBuf::from(DEFAULT_OUT_BASE).join(&args.fixture));

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

fn build_render_fixtures_command(repo_root: &Path, layout: &Layout, args: &PageLoopArgs) -> Result<Command> {
  let mut cmd = crate::cmd::cargo_agent_command(repo_root);
  cmd.current_dir(repo_root);
  cmd.env("FASTR_USE_BUNDLED_FONTS", "1");
  cmd
    .arg("run")
    .arg("--release")
    .args(["--bin", "render_fixtures", "--"]);
  cmd.arg("--fixtures-dir").arg(&layout.fixtures_dir);
  cmd.arg("--out-dir").arg(&layout.fastrender_dir);
  cmd.arg("--fixtures").arg(&layout.fixture_stem);
  cmd
    .arg("--viewport")
    .arg(format!("{}x{}", args.viewport.0, args.viewport.1));
  cmd.arg("--dpr").arg(args.dpr.to_string());
  cmd.arg("--media").arg(args.media.as_cli_value());
  if args.write_snapshot {
    cmd.arg("--write-snapshot");
  }
  Ok(cmd)
}

fn build_inspect_frag_overlay_command(
  repo_root: &Path,
  layout: &Layout,
  args: &PageLoopArgs,
) -> Result<Command> {
  let mut cmd = crate::cmd::cargo_agent_command(repo_root);
  cmd.current_dir(repo_root);
  cmd.env("FASTR_USE_BUNDLED_FONTS", "1");
  cmd
    .arg("run")
    .arg("--release")
    .args(["--bin", "inspect_frag", "--"]);
  cmd.arg(layout.fixture_html.as_os_str());
  cmd.arg("--render-overlay").arg(&layout.overlay_png);
  cmd
    .arg("--viewport")
    .arg(format!("{}x{}", args.viewport.0, args.viewport.1));
  cmd.arg("--dpr").arg(args.dpr.to_string());
  cmd.arg("--media").arg(args.media.as_cli_value());
  Ok(cmd)
}

fn build_chrome_baseline_command(repo_root: &Path, layout: &Layout, args: &PageLoopArgs) -> Result<Command> {
  let xtask = std::env::current_exe().context("resolve current xtask executable path")?;
  let mut cmd = Command::new(xtask);
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
    .arg("--media")
    .arg(args.media.as_cli_value());
  cmd.current_dir(repo_root);
  Ok(cmd)
}

fn build_diff_renders_build_command(repo_root: &Path) -> Command {
  let mut cmd = crate::cmd::cargo_agent_command(repo_root);
  cmd.current_dir(repo_root);
  cmd
    .arg("build")
    .arg("--release")
    .args(["--bin", "diff_renders"]);
  cmd
}

fn build_diff_renders_command(repo_root: &Path, layout: &Layout) -> Result<Command> {
  let diff_renders_exe = crate::diff_renders_executable(repo_root);
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
