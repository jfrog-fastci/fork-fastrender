use anyhow::{bail, Context, Result};
use clap::Args;
use serde::Deserialize;
use std::collections::{HashMap, HashSet};
use std::path::Path;

/// Fail CI if `openssl-sys` (and thus system OpenSSL headers) are pulled into the default
/// `fastrender` dependency graph.
///
/// This is primarily a hermeticity guard: agent/CI environments should be able to build the core
/// renderer without installing OpenSSL development packages.
#[derive(Args, Debug, Clone, Copy)]
pub struct LintNoOpenSslArgs {
  /// Also assert that `openssl-sys` is absent when building fastrender with all features enabled.
  ///
  /// CI builds `--all-features`, so this provides an extra guard against optional features pulling
  /// in system OpenSSL dependencies.
  #[arg(long)]
  pub all_features: bool,
}

#[derive(Debug, Deserialize)]
struct CargoMetadata {
  packages: Vec<CargoPackage>,
  resolve: Option<CargoResolve>,
}

#[derive(Debug, Deserialize)]
struct CargoPackage {
  name: String,
  id: String,
}

#[derive(Debug, Deserialize)]
struct CargoResolve {
  nodes: Vec<CargoNode>,
}

#[derive(Debug, Deserialize)]
struct CargoNode {
  id: String,
  deps: Vec<CargoNodeDep>,
}

#[derive(Debug, Deserialize)]
struct CargoNodeDep {
  pkg: String,
}

pub fn run_lint_no_openssl(repo_root: &Path, args: LintNoOpenSslArgs) -> Result<()> {
  check_openssl_sys_absent(repo_root, &[], "default")?;
  if args.all_features {
    check_openssl_sys_absent(repo_root, &["--all-features"], "all-features")?;
  }
  Ok(())
}

fn check_openssl_sys_absent(repo_root: &Path, metadata_args: &[&str], label: &str) -> Result<()> {
  // Use the agent wrapper so local invocations don't spawn unbounded cargo compilations.
  let mut cmd = crate::cmd::cargo_agent_command(repo_root);
  cmd.args(["metadata", "--locked", "--format-version", "1"]);
  cmd.args(metadata_args);
  cmd.current_dir(repo_root);

  let output = cmd
    .output()
    .with_context(|| format!("failed to run {:?}", cmd.get_program()))?;
  if !output.status.success() {
    bail!(
      "`cargo metadata` ({label}) failed with status {}.\nstdout:\n{}\nstderr:\n{}",
      output.status,
      String::from_utf8_lossy(&output.stdout),
      String::from_utf8_lossy(&output.stderr)
    );
  }

  let stdout = String::from_utf8(output.stdout).context("decode cargo metadata stdout")?;
  let metadata: CargoMetadata =
    serde_json::from_str(&stdout).context("parse cargo metadata JSON")?;
  let resolve = metadata
    .resolve
    .context("cargo metadata did not include a resolved dependency graph")?;

  let Some(fastrender_pkg) = metadata.packages.iter().find(|pkg| pkg.name == "fastrender") else {
    bail!("cargo metadata did not include a `fastrender` package entry");
  };

  let mut nodes_by_id: HashMap<&str, &CargoNode> = HashMap::new();
  for node in &resolve.nodes {
    nodes_by_id.insert(node.id.as_str(), node);
  }

  let mut packages_by_id: HashMap<&str, &CargoPackage> = HashMap::new();
  for pkg in &metadata.packages {
    packages_by_id.insert(pkg.id.as_str(), pkg);
  }

  // Traverse the resolved graph starting from `fastrender` so we only gate dependencies that
  // affect the core renderer (workspace members may have different constraints).
  let mut stack = vec![fastrender_pkg.id.as_str()];
  let mut visited: HashSet<&str> = HashSet::new();
  while let Some(id) = stack.pop() {
    if !visited.insert(id) {
      continue;
    }
    let Some(node) = nodes_by_id.get(id) else {
      continue;
    };
    for dep in &node.deps {
      stack.push(dep.pkg.as_str());
    }
  }

  let mut offenders: Vec<&str> = Vec::new();
  for id in &visited {
    let Some(pkg) = packages_by_id.get(id) else {
      continue;
    };
    if pkg.name == "openssl-sys" {
      offenders.push(pkg.name.as_str());
    }
  }

  if !offenders.is_empty() {
    bail!(
      "lint-no-openssl ({label}): forbidden dependency `openssl-sys` found in the fastrender build graph.\n\
       \n\
       This makes builds depend on system OpenSSL development headers.\n\
       Prefer a Rust TLS backend (e.g. reqwest rustls) for hermetic CI/agent builds.\n\
       \n\
       To debug:\n\
         cargo tree -p fastrender | rg openssl"
    );
  }

  println!("✓ lint-no-openssl ({label}): openssl-sys not present in fastrender dependency graph");
  Ok(())
}
