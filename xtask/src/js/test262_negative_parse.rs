use anyhow::{anyhow, bail, Context, Result};
use clap::Args;
use globset::{Glob, GlobSet, GlobSetBuilder};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use walkdir::WalkDir;

const DEFAULT_TEST262_DIR: &str = "vendor/ecma-rs/test262-semantic/data";
const DEFAULT_CURATED_SUITE_PATH: &str = "tests/js/test262_suites/curated.toml";
const DEFAULT_MANIFEST_PATH: &str = "tests/js/test262_manifest.toml";

const DEFAULT_OUT_SUITE_PATH: &str = "target/js/test262_negative_parse_suite.toml";
const DEFAULT_REPORT_PATH: &str = "target/js/test262_negative_parse.json";

const DEFAULT_TIMEOUT_SECS: u64 = 10;
const DEFAULT_JOBS_CAP: usize = 4;

const TIMEOUT_TOTAL_SECS: &str = "600";
const TIMEOUT_KILL_SECS: &str = "10";

const MISMATCH_PREFIX: &str =
  "negative expectation mismatch: expected parse SyntaxError, got runtime";

#[derive(Args, Debug)]
pub struct Test262NegativeParseArgs {
  /// Path to the committed suite file whose globs are expanded before filtering.
  #[arg(long, value_name = "PATH", default_value = DEFAULT_CURATED_SUITE_PATH)]
  pub suite: PathBuf,

  /// Path to a local checkout of the tc39/test262 repository.
  #[arg(long, value_name = "DIR", default_value = DEFAULT_TEST262_DIR)]
  pub test262_dir: PathBuf,

  /// Override the expectations manifest (skip/xfail/flaky) used by `test262-semantic`.
  #[arg(long, value_name = "PATH", default_value = DEFAULT_MANIFEST_PATH)]
  pub manifest: PathBuf,

  /// Where to write the generated suite file (TOML) containing only negative-parse ids.
  #[arg(long, value_name = "PATH", default_value = DEFAULT_OUT_SUITE_PATH)]
  pub out_suite: PathBuf,

  /// JSON report output path.
  #[arg(
    long,
    visible_alias = "report-path",
    value_name = "PATH",
    default_value = DEFAULT_REPORT_PATH
  )]
  pub report: PathBuf,

  /// Per-test timeout forwarded to `test262-semantic`.
  #[arg(long, default_value_t = DEFAULT_TIMEOUT_SECS, value_name = "SECS")]
  pub timeout_secs: u64,
}

pub fn run_test262_negative_parse(args: Test262NegativeParseArgs) -> Result<()> {
  if args.timeout_secs == 0 {
    bail!("--timeout-secs must be > 0");
  }

  let repo_root = crate::repo_root();
  let ecma_rs_root = repo_root.join("vendor/ecma-rs");
  if !ecma_rs_root.join("Cargo.toml").is_file() {
    bail!(
      "Missing vendor/ecma-rs (expected {}).",
      ecma_rs_root.join("Cargo.toml").display()
    );
  }

  let test262_dir = resolve_repo_path(&repo_root, &args.test262_dir);
  ensure_test262_dir(&repo_root, &test262_dir)?;

  let suite_path = resolve_repo_path(&repo_root, &args.suite);
  if !suite_path.is_file() {
    bail!("suite file {} does not exist", suite_path.display());
  }

  let manifest_path = resolve_repo_path(&repo_root, &args.manifest);
  if !manifest_path.is_file() {
    bail!(
      "expectations manifest {} does not exist",
      manifest_path.display()
    );
  }

  let report_path = resolve_repo_path(&repo_root, &args.report);
  if let Some(parent) = report_path.parent() {
    fs::create_dir_all(parent)
      .with_context(|| format!("failed to create report directory {}", parent.display()))?;
  }

  let out_suite_path = resolve_repo_path(&repo_root, &args.out_suite);
  if let Some(parent) = out_suite_path.parent() {
    fs::create_dir_all(parent)
      .with_context(|| format!("failed to create suite directory {}", parent.display()))?;
  }

  // 1) Expand the curated suite globs to a concrete id list.
  let discovered = discover_tests(&test262_dir)?;
  let suite = load_suite_from_path(&suite_path)?;
  let curated_ids = select_tests(&suite, &discovered)?;

  // 2) Filter down to negative parse SyntaxError tests (based on YAML frontmatter).
  let mut discovered_by_id: HashMap<&str, &DiscoveredTest> = HashMap::new();
  for test in &discovered {
    discovered_by_id.insert(test.id.as_str(), test);
  }

  let mut negative_parse_ids: Vec<String> = Vec::new();
  for id in &curated_ids {
    let test = discovered_by_id
      .get(id.as_str())
      .copied()
      .ok_or_else(|| anyhow!("selected id `{id}` was not discovered"))?;
    let raw = read_utf8_file(&test.path)?;
    let parsed =
      parse_test_source(&raw).with_context(|| format!("parse test262 frontmatter for {id}"))?;

    let Some(frontmatter) = parsed.frontmatter else {
      continue;
    };
    let Some(negative) = frontmatter.negative else {
      continue;
    };

    if negative.phase.eq_ignore_ascii_case("parse") && negative.typ == "SyntaxError" {
      negative_parse_ids.push(id.clone());
    }
  }
  negative_parse_ids.sort();
  negative_parse_ids.dedup();

  if negative_parse_ids.is_empty() {
    bail!(
      "no negative parse SyntaxError tests found after expanding suite {}",
      suite_path.display()
    );
  }

  println!("Expanded suite: {}", suite_path.display());
  println!("  curated ids: {}", curated_ids.len());
  println!(
    "  negative parse SyntaxError ids: {}",
    negative_parse_ids.len()
  );

  // 3) Write a temporary suite file containing only those ids.
  write_negative_parse_suite(&out_suite_path, &negative_parse_ids)?;
  println!("Generated suite: {}", out_suite_path.display());

  // 4) Rebuild test262-semantic before running the suite.
  //
  // We execute the binary directly (faster than `cargo run`), so we must ensure it's rebuilt on
  // every invocation to avoid stale-binary false negatives (where a previously-built runner is
  // executed after the JS engine has changed).
  println!();
  println!(
    "Rebuilding test262-semantic to avoid stale results (timeout -k {} {})...",
    TIMEOUT_KILL_SECS, TIMEOUT_TOTAL_SECS
  );
  let mut build_cmd = Command::new("timeout");
  build_cmd
    .args(["-k", TIMEOUT_KILL_SECS, TIMEOUT_TOTAL_SECS])
    .arg("bash")
    .arg(repo_root.join("scripts/cargo_agent.sh"))
    .arg("build")
    .args(["-p", "test262-semantic"]);
  build_cmd.current_dir(&repo_root);
  // Ensure the binary is built into `vendor/ecma-rs/target/` so we can execute it directly.
  build_cmd.env_remove("CARGO_TARGET_DIR");

  crate::print_command(&build_cmd);
  let build_status = build_cmd
    .status()
    .with_context(|| "failed to spawn `timeout` (coreutils) for building test262-semantic")?;
  if !build_status.success() {
    bail!("test262-semantic build failed with status {build_status}");
  }

  let exe_suffix = if cfg!(windows) { ".exe" } else { "" };
  let runner_rel_path = PathBuf::from(format!("target/debug/test262-semantic{exe_suffix}"));
  let runner_abs_path = ecma_rs_root.join(&runner_rel_path);
  if !runner_abs_path.is_file() {
    bail!(
      "test262-semantic binary not found at {} after rebuilding",
      runner_abs_path.display()
    );
  }

  // 5) Run test262-semantic on the generated suite under a hard timeout and write a JSON report.
  let jobs = crate::cpu_budget().min(DEFAULT_JOBS_CAP).max(1);

  let mut cmd = Command::new("timeout");
  cmd
    .args(["-k", TIMEOUT_KILL_SECS, TIMEOUT_TOTAL_SECS])
    // Run from `vendor/ecma-rs/` so `test262-semantic`'s default `--test262-dir
    // test262-semantic/data` works.
    .arg(&runner_rel_path)
    .arg("--test262-dir")
    .arg(&test262_dir)
    .args(["--harness", "test262"])
    .arg("--suite-path")
    .arg(&out_suite_path)
    .arg("--manifest")
    .arg(&manifest_path)
    .arg("--timeout-secs")
    .arg(args.timeout_secs.to_string())
    .arg("--jobs")
    .arg(jobs.to_string())
    .arg("--report-path")
    .arg(&report_path)
    .args(["--fail-on", "none"]);
  cmd.current_dir(&ecma_rs_root);

  println!();
  println!(
    "Running test262-semantic (negative parse SyntaxError only; timeout -k {} {})...",
    TIMEOUT_KILL_SECS, TIMEOUT_TOTAL_SECS
  );
  crate::print_command(&cmd);
  let status = cmd
    .status()
    .with_context(|| "failed to spawn `timeout` (coreutils) for test262-semantic")?;

  // The underlying runner should always write its report even on failures. Parse it to extract the
  // mismatch list we care about.
  if !report_path.is_file() {
    bail!(
      "test262 runner did not write report JSON to {} (status={status})",
      report_path.display()
    );
  }

  let report = super::test262_report::read_report(&report_path)
    .with_context(|| format!("load test262 report {}", report_path.display()))?;

  // 5) Emit a short summary + list mismatching ids for the parse-vs-runtime negative expectation mismatch.
  let mut mismatches: BTreeMap<String, BTreeSet<super::test262_report::Variant>> = BTreeMap::new();
  for result in &report.results {
    let Some(err) = result.error.as_deref() else {
      continue;
    };
    if err.starts_with(MISMATCH_PREFIX) {
      mismatches
        .entry(result.id.clone())
        .or_default()
        .insert(result.variant);
    }
  }

  let mismatch_cases: usize = mismatches.values().map(|variants| variants.len()).sum();

  println!();
  println!("test262 negative-parse report summary:");
  println!("  report: {}", report_path.display());
  println!("  generated suite: {}", out_suite_path.display());
  println!(
    "  cases: total={} passed={} failed={} timed_out={} skipped={}",
    report.summary.total,
    report.summary.passed,
    report.summary.failed,
    report.summary.timed_out,
    report.summary.skipped
  );
  println!(
    "  mismatches ({}*): {} case(s) across {} id(s)",
    MISMATCH_PREFIX,
    mismatch_cases,
    mismatches.len()
  );
  if !status.success() {
    println!("  runner status: {status}");
  }

  if !mismatches.is_empty() {
    println!();
    println!("Mismatching ids:");
    for (id, variants) in &mismatches {
      let variants = variants
        .iter()
        .map(|v| v.to_string())
        .collect::<Vec<_>>()
        .join(", ");
      println!("  - {id} ({variants})");
    }
  }

  if status.success() {
    Ok(())
  } else {
    bail!(
      "test262 runner exited with status {status}; see report {}",
      report_path.display()
    );
  }
}

fn resolve_repo_path(repo_root: &Path, path: &Path) -> PathBuf {
  if path.is_absolute() {
    path.to_path_buf()
  } else {
    repo_root.join(path)
  }
}

fn ensure_test262_dir(repo_root: &Path, test262_dir: &Path) -> Result<()> {
  let test_dir = test262_dir.join("test");
  let harness_dir = test262_dir.join("harness");
  if test_dir.is_dir() && harness_dir.is_dir() {
    return Ok(());
  }

  let default_dir = repo_root.join(DEFAULT_TEST262_DIR);
  if test262_dir == default_dir {
    bail!(
      "test262 semantic corpus is missing at {}.\n\
       Initialize it with:\n\
         git submodule update --init vendor/ecma-rs/test262-semantic/data\n\
       \n\
       See docs/js_test262.md for the full workflow.",
      test262_dir.display()
    );
  }

  bail!(
    "test262 checkout directory {} is missing required folders (expected {}/test and {}/harness)",
    test262_dir.display(),
    test262_dir.display(),
    test262_dir.display()
  );
}

#[derive(Debug, Clone)]
struct DiscoveredTest {
  id: String,
  path: PathBuf,
}

fn discover_tests(test262_dir: &Path) -> Result<Vec<DiscoveredTest>> {
  let test_dir = test262_dir.join("test");
  if !test_dir.is_dir() {
    bail!(
      "test262 test directory not found at {} (expected a tc39/test262 checkout)",
      test_dir.display()
    );
  }

  let mut out = Vec::new();
  for entry in WalkDir::new(&test_dir).follow_links(false) {
    let entry = entry.with_context(|| format!("walk {}", test_dir.display()))?;
    if !entry.file_type().is_file() {
      continue;
    }
    let path = entry.into_path();
    if path.extension().and_then(|ext| ext.to_str()) != Some("js") {
      continue;
    }
    // tc39/test262 stores various helper modules (fixtures) alongside tests. Exclude them so
    // `*_FIXTURE.js` doesn't get selected by suite globs.
    if path
      .file_name()
      .and_then(|name| name.to_str())
      .is_some_and(|name| name.contains("FIXTURE"))
    {
      continue;
    }

    let id = normalize_id(&test_dir, &path);
    out.push(DiscoveredTest { id, path });
  }

  if out.is_empty() {
    bail!("no tests discovered under {}", test_dir.display());
  }

  out.sort_by(|a, b| a.id.cmp(&b.id));
  Ok(out)
}

fn normalize_id(root: &Path, path: &Path) -> String {
  let mut id = path
    .strip_prefix(root)
    .unwrap_or(path)
    .to_string_lossy()
    .into_owned();
  if id.contains('\\') {
    id = id.replace('\\', "/");
  }
  id
}

fn read_utf8_file(path: &Path) -> Result<String> {
  fs::read_to_string(path).with_context(|| format!("read {}", path.display()))
}

#[derive(Debug, Clone, Default, Deserialize)]
struct Suite {
  #[serde(default)]
  tests: Vec<String>,
  #[serde(default)]
  include: Vec<String>,
  #[serde(default)]
  exclude: Vec<String>,
}

fn load_suite_from_path(path: &Path) -> Result<Suite> {
  let raw = fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
  toml::from_str::<Suite>(&raw).map_err(|err| anyhow!("{}: {err}", path.display()))
}

fn select_tests(suite: &Suite, available: &[DiscoveredTest]) -> Result<Vec<String>> {
  let available_ids: HashSet<&str> = available.iter().map(|t| t.id.as_str()).collect();
  let mut selected = BTreeSet::new();

  for id in &suite.tests {
    if !available_ids.contains(id.as_str()) {
      bail!("suite references missing test id `{id}`");
    }
    selected.insert(id.clone());
  }

  let include = compile_glob_set(&suite.include, "include")?;
  if let Some(include) = &include {
    for test in available {
      if include.is_match(&test.id) {
        selected.insert(test.id.clone());
      }
    }
  }

  let exclude = compile_glob_set(&suite.exclude, "exclude")?;
  if let Some(exclude) = &exclude {
    selected.retain(|id| !exclude.is_match(id));
  }

  if selected.is_empty() {
    bail!("suite selected zero tests");
  }

  Ok(selected.into_iter().collect())
}

fn compile_glob_set(patterns: &[String], label: &str) -> Result<Option<GlobSet>> {
  if patterns.is_empty() {
    return Ok(None);
  }

  let mut builder = GlobSetBuilder::new();
  for pattern in patterns {
    let glob =
      Glob::new(pattern).map_err(|err| anyhow!("invalid {label} glob '{pattern}': {err}"))?;
    builder.add(glob);
  }

  Ok(Some(
    builder
      .build()
      .map_err(|err| anyhow!("invalid {label} globs: {err}"))?,
  ))
}

#[derive(Debug, Serialize)]
struct SuiteOut {
  tests: Vec<String>,
}

fn write_negative_parse_suite(path: &Path, ids: &[String]) -> Result<()> {
  let suite = SuiteOut {
    tests: ids.to_vec(),
  };
  let toml = toml::to_string_pretty(&suite).context("serialize suite toml")?;
  fs::write(path, format!("{toml}\n").as_bytes())
    .with_context(|| format!("write {}", path.display()))?;
  Ok(())
}

// --------------------------------------------------------------------------
// test262 YAML frontmatter parsing
// --------------------------------------------------------------------------

#[derive(Debug, Clone, Default, Deserialize, PartialEq, Eq)]
struct Frontmatter {
  #[serde(default, deserialize_with = "string_or_seq")]
  #[allow(dead_code)]
  includes: Vec<String>,
  #[serde(default, deserialize_with = "string_or_seq")]
  #[allow(dead_code)]
  flags: Vec<String>,
  #[serde(default, deserialize_with = "string_or_seq")]
  #[allow(dead_code)]
  features: Vec<String>,
  #[serde(default)]
  negative: Option<Negative>,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
struct Negative {
  phase: String,
  #[serde(rename = "type")]
  typ: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct ParsedTestSource {
  frontmatter: Option<Frontmatter>,
  #[allow(dead_code)]
  body: String,
}

/// Parse `/*--- ... ---*/` test262 YAML frontmatter from the given source string.
///
/// This intentionally matches the upstream `test262-semantic` parser so we filter the same tests
/// that the runner will treat as `negative`.
fn parse_test_source(source: &str) -> Result<ParsedTestSource> {
  let source_no_bom = source.strip_prefix('\u{feff}').unwrap_or(source);
  let Some(start) = find_frontmatter_start(source_no_bom) else {
    return Ok(ParsedTestSource {
      frontmatter: None,
      body: source_no_bom.to_string(),
    });
  };

  let yaml_start = start + "/*---".len();
  let Some(end_rel) = source_no_bom[yaml_start..].find("---*/") else {
    bail!("frontmatter begins with `/*---` but is missing terminating `---*/`");
  };
  let yaml_end = yaml_start + end_rel;
  let yaml = &source_no_bom[yaml_start..yaml_end];
  let after = yaml_end + "---*/".len();

  let frontmatter: Frontmatter =
    serde_yaml::from_str(yaml).context("deserialize test262 YAML frontmatter")?;
  let is_raw = frontmatter.flags.iter().any(|flag| flag == "raw");

  Ok(ParsedTestSource {
    frontmatter: Some(frontmatter),
    body: if is_raw {
      // `raw` tests must not have their source modified, which includes
      // preserving the `/*--- ... ---*/` frontmatter comment.
      source.to_string()
    } else {
      let after = source_no_bom
        .get(after..)
        .ok_or_else(|| anyhow!("frontmatter terminator offset out of bounds"))?;

      // Preserve any leading whitespace/comments before the frontmatter block, but remove the
      // frontmatter block itself.
      let mut body = String::with_capacity(start + after.len());
      body.push_str(&source_no_bom[..start]);
      body.push_str(after);
      body
    },
  })
}

fn find_frontmatter_start(source: &str) -> Option<usize> {
  let mut i = 0usize;

  // Hashbang grammar (aka shebang). Only valid at the start of the file, after any BOM (already
  // stripped by `parse_test_source`).
  if source.as_bytes().starts_with(b"#!") {
    let Some(newline) = source.find('\n') else {
      return None;
    };
    i = newline + 1;
  }

  while i < source.len() {
    // Skip whitespace.
    while i < source.len() {
      let ch = source[i..].chars().next()?;
      if ch.is_whitespace() {
        i += ch.len_utf8();
      } else {
        break;
      }
    }

    if i >= source.len() {
      return None;
    }

    let rest = &source[i..];
    if rest.starts_with("//") {
      // Line comment, consume through newline (or EOF).
      let Some(newline_rel) = rest.find('\n') else {
        return None;
      };
      i += newline_rel + 1;
      continue;
    }

    if rest.starts_with("/*---") {
      return Some(i);
    }

    if rest.starts_with("/*") {
      // Block comment, consume through terminator.
      let Some(end_rel) = rest[2..].find("*/") else {
        // Unterminated comment: treat as "no frontmatter".
        return None;
      };
      i += 2 + end_rel + 2;
      continue;
    }

    // Non-whitespace / non-comment token before frontmatter => not frontmatter.
    return None;
  }

  None
}

fn string_or_seq<'de, D>(deserializer: D) -> std::result::Result<Vec<String>, D::Error>
where
  D: serde::de::Deserializer<'de>,
{
  struct Visitor;

  impl<'de> serde::de::Visitor<'de> for Visitor {
    type Value = Vec<String>;

    fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
      formatter.write_str("string or sequence of strings")
    }

    fn visit_str<E>(self, v: &str) -> std::result::Result<Self::Value, E>
    where
      E: serde::de::Error,
    {
      Ok(vec![v.to_string()])
    }

    fn visit_string<E>(self, v: String) -> std::result::Result<Self::Value, E>
    where
      E: serde::de::Error,
    {
      Ok(vec![v])
    }

    fn visit_seq<A>(self, mut seq: A) -> std::result::Result<Self::Value, A::Error>
    where
      A: serde::de::SeqAccess<'de>,
    {
      let mut out = Vec::new();
      while let Some(value) = seq.next_element::<String>()? {
        out.push(value);
      }
      Ok(out)
    }
  }

  deserializer.deserialize_any(Visitor)
}
