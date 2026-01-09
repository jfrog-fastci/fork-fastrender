use anyhow::{anyhow, bail, Result};
use fastrender::pageset::{pageset_entries_with_collisions, PagesetEntry, PagesetFilter};
use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Debug, Clone)]
pub struct FreezePageFixturePlanArgs {
  pub pages: Vec<String>,
  pub html_dir: PathBuf,
  pub asset_cache_dir: PathBuf,
  pub fixtures_root: PathBuf,
  pub bundle_out_dir: PathBuf,
  pub overwrite: bool,
  pub allow_missing_resources: bool,
  pub include_scripts: bool,
  pub user_agent: String,
  pub accept_language: String,
  pub viewport: (u32, u32),
  pub dpr: f32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandSpec {
  pub program: String,
  pub args: Vec<String>,
}

impl CommandSpec {
  pub fn to_command(&self) -> Command {
    let mut cmd = Command::new(&self.program);
    cmd.args(&self.args);
    cmd
  }
}

#[derive(Debug, Clone)]
pub struct FreezePageFixturePlanItem {
  pub url: String,
  /// Collision-aware cache stem used for cached HTML filenames and fixture directory names.
  pub cache_stem: String,
  pub fixture_name: String,
  pub bundle_path: PathBuf,
  pub bundle_command: CommandSpec,
}

#[derive(Debug, Clone)]
pub struct FreezePageFixturePlan {
  pub pages: Vec<FreezePageFixturePlanItem>,
}

pub fn plan_freeze_page_fixture(args: &FreezePageFixturePlanArgs) -> Result<FreezePageFixturePlan> {
  let raw_pages: Vec<String> = args
    .pages
    .iter()
    .map(|p| p.trim().to_string())
    .filter(|p| !p.is_empty())
    .collect();

  if raw_pages.is_empty() {
    bail!("no pages specified; pass --page <URL-or-stem> and/or --pages <csv>");
  }

  let filter = PagesetFilter::from_inputs(&raw_pages)
    .ok_or_else(|| anyhow!("provided pages did not contain any valid URL/stem values"))?;

  let (entries, _collisions) = pageset_entries_with_collisions();
  let selected: Vec<PagesetEntry> = entries
    .into_iter()
    .filter(|entry| filter.matches_entry(entry))
    .collect();

  let missing = filter.unmatched(&selected);
  if selected.is_empty() {
    if missing.is_empty() {
      bail!("no pages matched the provided filter");
    }
    bail!("unknown pages in filter: {}", missing.join(", "));
  }
  if !missing.is_empty() {
    bail!("unknown pages in filter: {}", missing.join(", "));
  }

  let mut pages = Vec::new();
  for entry in selected {
    let stem = entry.cache_stem.clone();
    let bundle_path = args.bundle_out_dir.join(format!("{stem}.tar"));
    pages.push(FreezePageFixturePlanItem {
      url: entry.url.clone(),
      cache_stem: stem.clone(),
      fixture_name: stem.clone(),
      bundle_command: build_bundle_page_cache_command(&stem, &bundle_path, args),
      bundle_path,
    });
  }

  Ok(FreezePageFixturePlan { pages })
}

fn build_bundle_page_cache_command(
  stem: &str,
  bundle_path: &Path,
  args: &FreezePageFixturePlanArgs,
) -> CommandSpec {
  let bundle_path = bundle_path.to_string_lossy().to_string();
  let html_dir = args.html_dir.to_string_lossy().to_string();
  let asset_cache_dir = args.asset_cache_dir.to_string_lossy().to_string();
  let viewport = format!("{}x{}", args.viewport.0, args.viewport.1);

  let mut cmd = vec![
    "scripts/cargo_agent.sh".to_string(),
    "run".to_string(),
    "--release".to_string(),
    "--features".to_string(),
    "disk_cache".to_string(),
    "--bin".to_string(),
    "bundle_page".to_string(),
    "--".to_string(),
    "cache".to_string(),
    stem.to_string(),
    "--out".to_string(),
    bundle_path,
    "--html-dir".to_string(),
    html_dir,
    "--asset-cache-dir".to_string(),
    asset_cache_dir,
    "--user-agent".to_string(),
    args.user_agent.clone(),
    "--accept-language".to_string(),
    args.accept_language.clone(),
    "--viewport".to_string(),
    viewport,
    "--dpr".to_string(),
    args.dpr.to_string(),
  ];

  if args.allow_missing_resources {
    cmd.push("--allow-missing".to_string());
  }
  if args.include_scripts {
    cmd.push("--bundle-scripts".to_string());
  }

  CommandSpec {
    program: "bash".to_string(),
    args: cmd,
  }
}
