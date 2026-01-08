use anyhow::{bail, Context, Result};
use clap::Args;
use fastrender::resource::{DEFAULT_ACCEPT_LANGUAGE, DEFAULT_USER_AGENT};
use std::fs;
use std::path::PathBuf;
use std::process::Command;

#[derive(Args, Debug)]
pub struct FreezePageFixtureArgs {
  /// Pageset page URL or cache stem to freeze (repeatable).
  #[arg(long, value_name = "URL_OR_STEM")]
  pub page: Vec<String>,

  /// Pageset page URLs/cache stems to freeze (comma-separated).
  #[arg(long, value_delimiter = ',')]
  pub pages: Option<Vec<String>>,

  /// Directory containing cached HTML (`*.html` + `*.html.meta`).
  #[arg(long, default_value = "fetches/html", value_name = "DIR")]
  pub html_dir: PathBuf,

  /// Disk-backed subresource cache directory (defaults to fetches/assets).
  #[arg(
    long,
    default_value = "fetches/assets",
    value_name = "DIR",
    visible_alias = "cache-dir"
  )]
  pub asset_cache_dir: PathBuf,

  /// Skip fetching/prefetching and only bundle/import from the existing caches.
  #[arg(long)]
  pub no_fetch: bool,

  /// Re-fetch cached HTML even if it already exists (ignored with --no-fetch).
  #[arg(long, conflicts_with = "no_fetch")]
  pub refresh: bool,

  /// Root directory for offline page fixtures.
  #[arg(long, default_value = "tests/pages/fixtures", value_name = "DIR")]
  pub fixtures_root: PathBuf,

  /// Where to write intermediate `bundle_page cache` archives.
  #[arg(
    long,
    default_value = "target/pageset_fixture_bundles",
    value_name = "DIR"
  )]
  pub bundle_out_dir: PathBuf,

  /// Allow replacing existing fixture directories when importing.
  #[arg(long)]
  pub overwrite: bool,

  /// Replace missing subresources with empty placeholder assets instead of failing the capture/import.
  #[arg(long)]
  pub allow_missing_resources: bool,

  /// Include `<script src>` resources in captured bundles and rewrite them to local fixture assets.
  ///
  /// This is intended for JS-enabled fixture capture. It can significantly increase bundle size.
  #[arg(long, visible_alias = "js-enabled")]
  pub include_scripts: bool,

  /// User-Agent header used for fetch/prefetch and for the disk cache namespace.
  #[arg(long, default_value = DEFAULT_USER_AGENT)]
  pub user_agent: String,

  /// Accept-Language header used for fetch/prefetch and for the disk cache namespace.
  #[arg(long, default_value = DEFAULT_ACCEPT_LANGUAGE)]
  pub accept_language: String,

  /// Viewport size as WxH (e.g. 1200x800; propagated to prefetch and bundle steps).
  #[arg(long, value_parser = crate::parse_viewport, default_value = "1200x800")]
  pub viewport: (u32, u32),

  /// Device pixel ratio for media queries/srcset (propagated to prefetch and bundle steps).
  #[arg(long, default_value = "1.0")]
  pub dpr: f32,
}

pub fn run_freeze_page_fixture(mut args: FreezePageFixtureArgs) -> Result<()> {
  let repo_root = crate::repo_root();

  if !args.html_dir.is_absolute() {
    args.html_dir = repo_root.join(&args.html_dir);
  }
  if !args.asset_cache_dir.is_absolute() {
    args.asset_cache_dir = repo_root.join(&args.asset_cache_dir);
  }
  if !args.fixtures_root.is_absolute() {
    args.fixtures_root = repo_root.join(&args.fixtures_root);
  }
  if !args.bundle_out_dir.is_absolute() {
    args.bundle_out_dir = repo_root.join(&args.bundle_out_dir);
  }

  let mut pages = args.page.clone();
  if let Some(extra) = &args.pages {
    pages.extend(extra.iter().cloned());
  }

  if pages.iter().all(|p| p.trim().is_empty()) {
    bail!("no pages specified; pass --page <URL-or-stem> and/or --pages <csv>");
  }

  let plan = xtask::freeze_page_fixture::plan_freeze_page_fixture(
    &xtask::freeze_page_fixture::FreezePageFixturePlanArgs {
      pages,
      html_dir: args.html_dir.clone(),
      asset_cache_dir: args.asset_cache_dir.clone(),
      fixtures_root: args.fixtures_root.clone(),
      bundle_out_dir: args.bundle_out_dir.clone(),
      overwrite: args.overwrite,
      allow_missing_resources: args.allow_missing_resources,
      include_scripts: args.include_scripts,
      user_agent: args.user_agent.clone(),
      accept_language: args.accept_language.clone(),
      viewport: args.viewport,
      dpr: args.dpr,
    },
  )?;

  let selected_cache_stems: Vec<String> = plan.pages.iter().map(|p| p.cache_stem.clone()).collect();
  let pages_csv = selected_cache_stems.join(",");

  let default_html_dir = repo_root.join("fetches/html");
  if !args.no_fetch && args.html_dir != default_html_dir {
    bail!(
      "--html-dir is only supported with --no-fetch because fetch_pages/prefetch_assets always use {}",
      default_html_dir.display()
    );
  }

  if args.no_fetch {
    ensure_cached_inputs_exist(&plan, &args)?;
  } else {
    run_fetch_pages_step(&args, &pages_csv)?;
    run_prefetch_assets_step(&args, &pages_csv)?;
  }

  if !plan.pages.is_empty() {
    fs::create_dir_all(&args.bundle_out_dir).with_context(|| {
      format!(
        "failed to create bundle output directory {}",
        args.bundle_out_dir.display()
      )
    })?;
  }

  for capture in &plan.pages {
    remove_path_if_exists(&capture.bundle_path)?;

    println!("Capturing bundle for {}...", capture.cache_stem);
    let mut bundle_cmd = capture.bundle_command.to_command();
    bundle_cmd.current_dir(&repo_root);
    crate::run_command(bundle_cmd)
      .with_context(|| format!("bundle_page cache {}", capture.cache_stem))?;

    println!("Importing fixture {}...", capture.fixture_name);
    let mut import_cmd = build_import_page_fixture_command(&args, capture);
    import_cmd.current_dir(&repo_root);
    crate::run_command(import_cmd)
      .with_context(|| format!("import-page-fixture {}", capture.fixture_name))?;
  }

  // Ensure the imported fixtures are fully offline unless the caller explicitly bypassed the
  // invariant.
  let mut validate_cmd = build_validate_page_fixtures_command(&args, &selected_cache_stems);
  validate_cmd.current_dir(&repo_root);
  crate::run_command(validate_cmd).context("validate-page-fixtures")?;

  Ok(())
}

fn ensure_cached_inputs_exist(
  plan: &xtask::freeze_page_fixture::FreezePageFixturePlan,
  args: &FreezePageFixtureArgs,
) -> Result<()> {
  if !args.asset_cache_dir.is_dir() {
    bail!(
      "asset cache directory {} does not exist; re-run without --no-fetch to warm it (or pass --asset-cache-dir)",
      args.asset_cache_dir.display()
    );
  }

  for capture in &plan.pages {
    let html_path = args.html_dir.join(format!("{}.html", capture.cache_stem));
    if !html_path.is_file() {
      bail!(
        "cached HTML {} is missing; re-run without --no-fetch (or pass --html-dir to point at an existing cache)",
        html_path.display()
      );
    }
  }

  Ok(())
}

fn run_fetch_pages_step(args: &FreezePageFixtureArgs, pages_csv: &str) -> Result<()> {
  let repo_root = crate::repo_root();
  let mut cmd = Command::new("cargo");
  cmd
    .arg("run")
    .arg("--release")
    .args(["--bin", "fetch_pages", "--"])
    .args(["--pages", pages_csv])
    .arg("--allow-collisions")
    .args(["--user-agent", &args.user_agent])
    .args(["--accept-language", &args.accept_language]);

  if args.refresh {
    cmd.arg("--refresh");
  }

  cmd.current_dir(&repo_root);
  crate::run_command(cmd).context("fetch_pages")?;
  Ok(())
}

fn run_prefetch_assets_step(args: &FreezePageFixtureArgs, pages_csv: &str) -> Result<()> {
  let repo_root = crate::repo_root();
  let mut cmd = build_prefetch_assets_command(args, pages_csv);
  cmd.current_dir(&repo_root);
  crate::run_command(cmd).context("prefetch_assets")?;
  Ok(())
}

fn build_prefetch_assets_command(args: &FreezePageFixtureArgs, pages_csv: &str) -> Command {
  use crate::DiskCacheFeatureExt;

  let mut cmd = Command::new("cargo");
  cmd
    .arg("run")
    .arg("--release")
    .apply_disk_cache_feature(true)
    .args(["--bin", "prefetch_assets", "--"])
    .arg("--cache-dir")
    .arg(&args.asset_cache_dir)
    .args(["--pages", pages_csv])
    .args(["--user-agent", &args.user_agent])
    .args(["--accept-language", &args.accept_language])
    .args([
      "--viewport",
      &format!("{}x{}", args.viewport.0, args.viewport.1),
    ])
    .args(["--dpr", &args.dpr.to_string()])
    .arg("--prefetch-images")
    .arg("--prefetch-css-url-assets");

  if args.include_scripts {
    cmd.arg("--prefetch-scripts");
  }

  if std::env::var_os("FASTR_DISK_CACHE_ALLOW_NO_STORE").is_none() {
    cmd.arg("--disk-cache-allow-no-store");
  }
  cmd
}

fn build_import_page_fixture_command(
  args: &FreezePageFixtureArgs,
  capture: &xtask::freeze_page_fixture::FreezePageFixturePlanItem,
) -> Command {
  let mut cmd = Command::new("cargo");
  cmd
    .arg("xtask")
    .arg("import-page-fixture")
    .arg(&capture.bundle_path)
    .arg(&capture.fixture_name)
    .arg("--output-root")
    .arg(&args.fixtures_root);

  if args.overwrite {
    cmd.arg("--overwrite");
  }
  if args.allow_missing_resources {
    cmd.arg("--allow-missing");
  }
  if args.include_scripts {
    cmd.arg("--rewrite-scripts");
  }

  cmd
}

fn build_validate_page_fixtures_command(
  args: &FreezePageFixtureArgs,
  selected_cache_stems: &[String],
) -> Command {
  let only_csv = selected_cache_stems.join(",");
  let mut cmd = Command::new("cargo");
  cmd
    .arg("xtask")
    .arg("validate-page-fixtures")
    .arg("--fixtures-root")
    .arg(&args.fixtures_root)
    .arg("--only")
    .arg(only_csv);

  if args.include_scripts {
    cmd.arg("--include-scripts");
  }

  cmd
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn include_scripts_plumbs_script_flags_to_commands() {
    let repo_root = crate::repo_root();

    let args = FreezePageFixtureArgs {
      page: vec!["example.com".to_string()],
      pages: None,
      html_dir: repo_root.join("fetches/html"),
      asset_cache_dir: repo_root.join("fetches/assets"),
      no_fetch: true,
      refresh: false,
      fixtures_root: repo_root.join("tests/pages/fixtures"),
      bundle_out_dir: repo_root.join("target/pageset_fixture_bundles"),
      overwrite: false,
      allow_missing_resources: false,
      include_scripts: true,
      user_agent: DEFAULT_USER_AGENT.to_string(),
      accept_language: DEFAULT_ACCEPT_LANGUAGE.to_string(),
      viewport: (1200, 800),
      dpr: 1.0,
    };

    let plan = xtask::freeze_page_fixture::plan_freeze_page_fixture(
      &xtask::freeze_page_fixture::FreezePageFixturePlanArgs {
        pages: vec!["example.com".to_string()],
        html_dir: args.html_dir.clone(),
        asset_cache_dir: args.asset_cache_dir.clone(),
        fixtures_root: args.fixtures_root.clone(),
        bundle_out_dir: args.bundle_out_dir.clone(),
        overwrite: args.overwrite,
        allow_missing_resources: args.allow_missing_resources,
        include_scripts: args.include_scripts,
        user_agent: args.user_agent.clone(),
        accept_language: args.accept_language.clone(),
        viewport: args.viewport,
        dpr: args.dpr,
      },
    )
    .expect("plan");

    assert_eq!(plan.pages.len(), 1);
    let capture = &plan.pages[0];
    assert!(
      capture
        .bundle_command
        .args
        .iter()
        .any(|a| a == "--bundle-scripts"),
      "bundle_page cache should include --bundle-scripts when --include-scripts is set: {:?}",
      capture.bundle_command.args
    );

    let prefetch_cmd = build_prefetch_assets_command(&args, "example.com");
    let prefetch_args: Vec<String> = prefetch_cmd
      .get_args()
      .map(|s| s.to_string_lossy().to_string())
      .collect();
    assert!(
      prefetch_args.iter().any(|a| a == "--prefetch-scripts"),
      "prefetch_assets should include --prefetch-scripts when --include-scripts is set: {prefetch_args:?}"
    );

    let import_cmd = build_import_page_fixture_command(&args, capture);
    let import_args: Vec<String> = import_cmd
      .get_args()
      .map(|s| s.to_string_lossy().to_string())
      .collect();
    assert!(
      import_args.iter().any(|a| a == "--rewrite-scripts"),
      "import-page-fixture should include --rewrite-scripts when --include-scripts is set: {import_args:?}"
    );

    let validate_cmd = build_validate_page_fixtures_command(&args, &[capture.cache_stem.clone()]);
    let validate_args: Vec<String> = validate_cmd
      .get_args()
      .map(|s| s.to_string_lossy().to_string())
      .collect();
    assert!(
      validate_args.iter().any(|a| a == "--include-scripts"),
      "validate-page-fixtures should include --include-scripts when --include-scripts is set: {validate_args:?}"
    );
  }
}

fn remove_path_if_exists(path: &std::path::Path) -> Result<()> {
  if path.is_dir() {
    fs::remove_dir_all(path).with_context(|| format!("remove {}", path.display()))?;
  } else if path.exists() {
    fs::remove_file(path).with_context(|| format!("remove {}", path.display()))?;
  }
  Ok(())
}
