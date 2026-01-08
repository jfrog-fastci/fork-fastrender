use anyhow::Result;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct CaptureMissingFailureFixturesArgs {
  pub progress_dir: PathBuf,
  pub fixtures_root: PathBuf,
  pub bundle_out_dir: PathBuf,
  pub asset_cache_dir: PathBuf,
  pub user_agent: Option<String>,
  pub accept_language: Option<String>,
  pub viewport: Option<String>,
  pub dpr: Option<String>,
  pub allow_missing_resources: bool,
  pub overwrite: bool,
  pub include_scripts: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandSpec {
  pub program: String,
  pub args: Vec<String>,
}

impl CommandSpec {
  pub fn to_command(&self) -> std::process::Command {
    let mut cmd = std::process::Command::new(&self.program);
    cmd.args(&self.args);
    cmd
  }
}

#[derive(Debug, Clone)]
pub struct CaptureMissingFailureFixturePlan {
  pub stem: String,
  pub bundle_path: PathBuf,
  pub bundle_command: CommandSpec,
  pub import_command: CommandSpec,
}

#[derive(Debug, Clone)]
pub struct CaptureMissingFailureFixturesPlan {
  pub failing_pages_total: usize,
  pub fixtures_already_present: usize,
  pub captures: Vec<CaptureMissingFailureFixturePlan>,
}

pub fn plan_capture_missing_failure_fixtures(
  args: &CaptureMissingFailureFixturesArgs,
) -> Result<CaptureMissingFailureFixturesPlan> {
  let mut captures = Vec::new();

  let plan = crate::pageset_failure_fixtures::plan_missing_failure_fixtures(
    &args.progress_dir,
    &args.fixtures_root,
  )?;
  for page in &plan.missing_fixtures {
    let bundle_path = args.bundle_out_dir.join(format!("{}.tar", page.stem));
    captures.push(CaptureMissingFailureFixturePlan {
      stem: page.stem.clone(),
      bundle_path: bundle_path.clone(),
      bundle_command: build_bundle_page_cache_command(&page.stem, &bundle_path, args),
      import_command: build_import_page_fixture_command(&page.stem, &bundle_path, args),
    });
  }

  Ok(CaptureMissingFailureFixturesPlan {
    failing_pages_total: plan.failing_pages.len(),
    fixtures_already_present: plan.existing_fixtures.len(),
    captures,
  })
}

fn build_bundle_page_cache_command(
  stem: &str,
  bundle_path: &Path,
  args: &CaptureMissingFailureFixturesArgs,
) -> CommandSpec {
  let bundle_path = bundle_path.to_string_lossy().to_string();
  let cache_dir = args.asset_cache_dir.to_string_lossy().to_string();

  let mut cmd = vec![
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
    "--asset-cache-dir".to_string(),
    cache_dir,
  ];

  if args.allow_missing_resources {
    cmd.push("--allow-missing".to_string());
  }
  if let Some(user_agent) = args.user_agent.as_deref() {
    cmd.push("--user-agent".to_string());
    cmd.push(user_agent.to_string());
  }
  if let Some(accept_language) = args.accept_language.as_deref() {
    cmd.push("--accept-language".to_string());
    cmd.push(accept_language.to_string());
  }
  if let Some(viewport) = args.viewport.as_deref() {
    cmd.push("--viewport".to_string());
    cmd.push(viewport.to_string());
  }
  if let Some(dpr) = args.dpr.as_deref() {
    cmd.push("--dpr".to_string());
    cmd.push(dpr.to_string());
  }
  if args.include_scripts {
    cmd.push("--bundle-scripts".to_string());
  }

  CommandSpec {
    program: "cargo".to_string(),
    args: cmd,
  }
}

fn build_import_page_fixture_command(
  stem: &str,
  bundle_path: &Path,
  args: &CaptureMissingFailureFixturesArgs,
) -> CommandSpec {
  let bundle_path = bundle_path.to_string_lossy().to_string();
  let fixtures_root = args.fixtures_root.to_string_lossy().to_string();

  let mut cmd = vec![
    "xtask".to_string(),
    "import-page-fixture".to_string(),
    bundle_path,
    stem.to_string(),
    "--output-root".to_string(),
    fixtures_root,
  ];

  if args.overwrite {
    cmd.push("--overwrite".to_string());
  }
  if args.allow_missing_resources {
    cmd.push("--allow-missing".to_string());
  }
  if args.include_scripts {
    cmd.push("--rewrite-scripts".to_string());
  }

  CommandSpec {
    program: "cargo".to_string(),
    args: cmd,
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn include_scripts_propagates_to_bundle_and_import_commands() {
    let args = CaptureMissingFailureFixturesArgs {
      progress_dir: PathBuf::from("progress"),
      fixtures_root: PathBuf::from("fixtures"),
      bundle_out_dir: PathBuf::from("bundles"),
      asset_cache_dir: PathBuf::from("fetches/assets"),
      user_agent: None,
      accept_language: None,
      viewport: None,
      dpr: None,
      allow_missing_resources: false,
      overwrite: false,
      include_scripts: true,
    };

    let bundle_cmd = build_bundle_page_cache_command("example.com", Path::new("out.tar"), &args);
    assert!(
      bundle_cmd.args.iter().any(|a| a == "--bundle-scripts"),
      "expected bundle_page cache command to include --bundle-scripts when include_scripts is set: {:?}",
      bundle_cmd.args
    );

    let import_cmd = build_import_page_fixture_command("example.com", Path::new("out.tar"), &args);
    assert!(
      import_cmd.args.iter().any(|a| a == "--rewrite-scripts"),
      "expected import-page-fixture command to include --rewrite-scripts when include_scripts is set: {:?}",
      import_cmd.args
    );
  }
}
