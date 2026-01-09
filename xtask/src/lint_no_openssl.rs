use anyhow::{bail, Context, Result};
use clap::Args;
use serde::Deserialize;
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::Path;

/// Fail CI if `openssl-sys` (and thus system OpenSSL headers) are pulled into the dependency graph.
///
/// This is primarily a hermeticity guard: agent/CI environments should be able to build the core
/// renderer without installing OpenSSL development packages.
#[derive(Args, Debug, Clone, Copy)]
pub struct LintNoOpenSslArgs {
  /// Check the resolved dependency graph for the entire Cargo workspace (all workspace members),
  /// not just the `fastrender` crate.
  ///
  /// This is a stronger hermeticity guard: CI runs `cargo test --all-features` at the workspace
  /// root, which compiles all workspace members.
  #[arg(long)]
  pub workspace: bool,

  /// Also assert that `openssl-sys` is absent when building with all features enabled.
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
  workspace_members: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct CargoPackage {
  name: String,
  version: String,
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
  let scope = if args.workspace {
    Scope::Workspace
  } else {
    Scope::Fastrender
  };

  check_openssl_sys_absent(repo_root, &[], "default", scope)?;
  if args.all_features {
    check_openssl_sys_absent(repo_root, &["--all-features"], "all-features", scope)?;
  }
  Ok(())
}

#[derive(Debug, Clone, Copy)]
enum Scope {
  Fastrender,
  Workspace,
}

impl Scope {
  fn label(self) -> &'static str {
    match self {
      Scope::Fastrender => "fastrender",
      Scope::Workspace => "workspace",
    }
  }
}

fn check_openssl_sys_absent(
  repo_root: &Path,
  metadata_args: &[&str],
  label: &str,
  scope: Scope,
) -> Result<()> {
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

  let fastrender_pkg = metadata.packages.iter().find(|pkg| pkg.name == "fastrender");
  if matches!(scope, Scope::Fastrender) && fastrender_pkg.is_none() {
    bail!("cargo metadata did not include a `fastrender` package entry");
  }

  let mut nodes_by_id: HashMap<&str, &CargoNode> = HashMap::new();
  for node in &resolve.nodes {
    nodes_by_id.insert(node.id.as_str(), node);
  }

  let mut packages_by_id: HashMap<&str, &CargoPackage> = HashMap::new();
  for pkg in &metadata.packages {
    packages_by_id.insert(pkg.id.as_str(), pkg);
  }

  // Traverse the resolved graph starting from the requested root(s).
  //
  // Keep a parent map so we can print a useful dependency chain if `openssl-sys` shows up.
  let root_ids: Vec<&str> = match scope {
    Scope::Fastrender => vec![fastrender_pkg.unwrap().id.as_str()],
    Scope::Workspace => metadata.workspace_members.iter().map(|id| id.as_str()).collect(),
  };

  let mut queue: VecDeque<&str> = VecDeque::new();
  let mut visited: HashSet<&str> = HashSet::new();
  let mut parent: HashMap<&str, &str> = HashMap::new();

  for root_id in root_ids {
    if visited.insert(root_id) {
      queue.push_back(root_id);
    }
  }

  while let Some(id) = queue.pop_front() {
    let Some(node) = nodes_by_id.get(id) else {
      continue;
    };
    for dep in &node.deps {
      let dep_id = dep.pkg.as_str();
      if visited.insert(dep_id) {
        parent.insert(dep_id, id);
        queue.push_back(dep_id);
      }
    }
  }

  let mut offenders: Vec<&str> = Vec::new();
  for id in visited.iter().copied() {
    let Some(pkg) = packages_by_id.get(id) else {
      continue;
    };
    if pkg.name == "openssl-sys" {
      offenders.push(id);
    }
  }

  if !offenders.is_empty() {
    let mut chains = Vec::new();
    for offender_id in &offenders {
      let mut chain_ids = Vec::new();
      let mut cur = *offender_id;
      chain_ids.push(cur);
      while let Some(prev) = parent.get(cur).copied() {
        chain_ids.push(prev);
        cur = prev;
      }
      chain_ids.reverse();

      let mut labels = Vec::new();
      for id in chain_ids {
        let label = packages_by_id
          .get(id)
          .map(|pkg| format!("{}@{}", pkg.name, pkg.version))
          .unwrap_or_else(|| id.to_string());
        labels.push(label);
      }
      chains.push(labels.join(" -> "));
    }

    bail!(
      "lint-no-openssl ({label}, {}): forbidden dependency `openssl-sys` found in the resolved build graph.\n\
       \n\
       This makes builds depend on system OpenSSL development headers.\n\
       Prefer a Rust TLS backend (e.g. reqwest rustls) for hermetic CI/agent builds.\n\
       \n\
       Dependency path(s):\n\
       {}\n\
       \n\
       To debug:\n\
         cargo tree -i openssl-sys"
      ,
      scope.label(),
      chains
        .iter()
        .map(|chain| format!("  - {chain}"))
        .collect::<Vec<_>>()
        .join("\n")
    );
  }

  println!(
    "✓ lint-no-openssl ({label}, {}): openssl-sys not present in resolved dependency graph",
    scope.label()
  );
  Ok(())
}
