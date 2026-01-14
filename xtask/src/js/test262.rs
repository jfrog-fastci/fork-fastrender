use anyhow::{bail, Context, Result};
use clap::{Args, ValueEnum};
pub use conformance_harness::FailOn;
use std::fs;
use std::path::{Path, PathBuf};

const DEFAULT_TEST262_DIR: &str = "vendor/ecma-rs/test262-semantic/data";
const DEFAULT_REPORT_PATH: &str = "target/js/test262.json";
const DEFAULT_SUMMARY_PATH: &str = "target/js/test262_summary.md";
const DEFAULT_MANIFEST_PATH: &str = "tests/js/test262_manifest.toml";
const DEFAULT_CURATED_SUITE_PATH: &str = "tests/js/test262_suites/curated.toml";
const DEFAULT_SMOKE_SUITE_PATH: &str = "tests/js/test262_suites/smoke.toml";
const DEFAULT_MODULES_SMOKE_SUITE_PATH: &str = "tests/js/test262_suites/modules_smoke.toml";
const DEFAULT_LANGUAGE_STATEMENTS_SUITE_PATH: &str = "tests/js/test262_suites/language_statements.toml";
const DEFAULT_LANGUAGE_FUNCTIONS_SUITE_PATH: &str = "tests/js/test262_suites/language_functions.toml";
const DEFAULT_LANGUAGE_CLASSES_SUITE_PATH: &str = "tests/js/test262_suites/language_classes.toml";
const DEFAULT_LANGUAGE_SCOPES_SUITE_PATH: &str = "tests/js/test262_suites/language_scopes.toml";
const DEFAULT_BUILTINS_CORE_SUITE_PATH: &str = "tests/js/test262_suites/builtins_core.toml";
const DEFAULT_BUILTINS_JSON_MATH_SUITE_PATH: &str = "tests/js/test262_suites/builtins_json_math.toml";
const DEFAULT_REGEXP_SUITE_PATH: &str = "tests/js/test262_suites/regexp.toml";
const DEFAULT_REGEXP_UNICODE_SETS_SUITE_PATH: &str = "tests/js/test262_suites/regexp_unicode_sets.toml";
const DEFAULT_REGEXP_PROPERTY_ESCAPES_GENERATED_SUITE_PATH: &str =
  "tests/js/test262_suites/regexp_property_escapes_generated.toml";
const DEFAULT_REGEXP_LOOKBEHIND_SUITE_PATH: &str = "tests/js/test262_suites/regexp_lookbehind.toml";
const DEFAULT_BASELINE_PATH: &str = "progress/test262/baseline.json";

const DEFAULT_TIMEOUT_SECS: u64 = 10;
const DEFAULT_JOBS_CAP: usize = 4;

#[derive(Copy, Clone, Debug, Eq, PartialEq, ValueEnum)]
#[clap(rename_all = "lowercase")]
pub enum HarnessMode {
  /// Prepend the standard tc39/test262 harness (`assert.js`, `sta.js`) plus any additional
  /// frontmatter `includes` files.
  #[value(aliases = ["upstream", "full"])]
  Test262,
  /// Prepend only the harness files explicitly listed in test frontmatter (`includes`).
  ///
  /// This is useful when you want to avoid implicitly loading `assert.js`/`sta.js`.
  ///
  /// Alias: `minimal` (kept for backwards compatibility with older FastRender docs/CLI).
  #[value(aliases = ["minimal"])]
  Includes,
  /// Do not prepend any harness code (test body only).
  None,
}

impl HarnessMode {
  fn as_cli_value(self) -> &'static str {
    match self {
      Self::Test262 => "test262",
      Self::Includes => "includes",
      Self::None => "none",
    }
  }
}

#[derive(Copy, Clone, Debug, Eq, PartialEq, ValueEnum)]
#[clap(rename_all = "snake_case")]
pub enum Test262Suite {
  /// Default curated suite (CI-friendly, deterministic subset).
  Curated,
  /// Minimal suite intended for quick wiring/smoke checks.
  Smoke,
  /// Module-focused smoke suite (import/export/import.meta/top-level await/dynamic import/JSON modules).
  #[value(aliases = ["modules", "modules-smoke"])]
  ModulesSmoke,
  /// RegExp engine focused subset (named groups, indices, lookbehind, property escapes, etc).
  Regexp,
  /// RegExp `/v` (Unicode sets) focused subset.
  RegexpUnicodeSets,
  /// RegExp Unicode property escapes (generated corpus) focused subset.
  RegexpPropertyEscapesGenerated,
  /// Targeted suite for RegExp lookbehind support (regexp-lookbehind).
  #[value(aliases = ["regexplookbehind"])]
  RegexpLookbehind,
  /// Statement/control-flow focused subset of the curated suite.
  #[value(aliases = ["language-statements"])]
  LanguageStatements,
  /// Function-focused subset of the curated suite.
  #[value(aliases = ["language-functions"])]
  LanguageFunctions,
  /// Class-focused subset of the curated suite.
  #[value(aliases = ["language-classes"])]
  LanguageClasses,
  /// Lexical scope + directive prologue focused subset of the curated suite.
  #[value(aliases = ["language-scopes"])]
  LanguageScopes,
  /// Core built-ins subset of the curated suite (Object/Array/String/Number/Boolean/Symbol).
  #[value(aliases = ["builtins-core"])]
  BuiltinsCore,
  /// JSON + Math built-ins subset of the curated suite.
  #[value(aliases = ["builtins-json-math"])]
  BuiltinsJsonMath,
}

#[derive(Args, Debug)]
pub struct Test262Args {
  /// Select which preset suite to run.
  #[arg(long, value_enum, default_value_t = Test262Suite::Curated)]
  pub suite: Test262Suite,

  /// Configure which tc39/test262 harness scripts are prepended before each test body.
  ///
  /// FastRender defaults to `test262`, matching the upstream runner default.
  #[arg(long, value_enum, default_value_t = HarnessMode::Test262)]
  pub harness: HarnessMode,

  /// Override the expectations manifest (skip/xfail/flaky) used to classify known gaps.
  #[arg(long, value_name = "PATH", default_value = DEFAULT_MANIFEST_PATH)]
  pub manifest: PathBuf,

  /// Run only a deterministic shard of the corpus (index/total, 0-based).
  #[arg(long, value_parser = crate::parse_shard)]
  pub shard: Option<(usize, usize)>,

  /// Per-test timeout (seconds).
  #[arg(long, default_value_t = DEFAULT_TIMEOUT_SECS, value_name = "SECS")]
  pub timeout_secs: u64,

  /// Control which mismatches cause a non-zero exit code.
  #[arg(long, default_value_t = FailOn::New, value_enum)]
  pub fail_on: FailOn,

  /// JSON report output path.
  #[arg(
    long,
    visible_alias = "report-path",
    value_name = "PATH",
    default_value = DEFAULT_REPORT_PATH
  )]
  pub report: PathBuf,

  /// Write a human-readable Markdown summary alongside the JSON report.
  #[arg(long, value_name = "PATH", default_value = DEFAULT_SUMMARY_PATH)]
  pub summary: PathBuf,

  /// Path to the committed baseline snapshot used for monotonic-progress enforcement.
  #[arg(long, value_name = "PATH", default_value = DEFAULT_BASELINE_PATH)]
  pub baseline: PathBuf,

  /// Refresh the committed baseline snapshot (and its summaries) intentionally.
  #[arg(long)]
  pub update_baseline: bool,

  /// Disable baseline regression/timeouts gating (useful for local iteration).
  #[arg(long)]
  pub no_gate: bool,

  /// Glob or regex to filter tests by id (after suite selection).
  #[arg(long, value_name = "FILTER")]
  pub filter: Option<String>,

  /// Path to a local checkout of the tc39/test262 repository.
  #[arg(long, value_name = "DIR", default_value = DEFAULT_TEST262_DIR)]
  pub test262_dir: PathBuf,

  /// Extra arguments forwarded to the ecma-rs `test262-semantic` runner (use `--` before these).
  #[arg(last = true)]
  pub extra: Vec<String>,
}

pub fn run_test262(args: Test262Args) -> Result<()> {
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
  ensure_test262_dir(&repo_root, &test262_dir, args.harness)?;

  let manifest_path = resolve_repo_path(&repo_root, &args.manifest);
  if !manifest_path.is_file() {
    bail!(
      "expectations manifest {} does not exist",
      manifest_path.display()
    );
  }

  let suite_path = repo_root.join(match args.suite {
    Test262Suite::Curated => DEFAULT_CURATED_SUITE_PATH,
    Test262Suite::Smoke => DEFAULT_SMOKE_SUITE_PATH,
    Test262Suite::ModulesSmoke => DEFAULT_MODULES_SMOKE_SUITE_PATH,
    Test262Suite::Regexp => DEFAULT_REGEXP_SUITE_PATH,
    Test262Suite::RegexpUnicodeSets => DEFAULT_REGEXP_UNICODE_SETS_SUITE_PATH,
    Test262Suite::RegexpPropertyEscapesGenerated => DEFAULT_REGEXP_PROPERTY_ESCAPES_GENERATED_SUITE_PATH,
    Test262Suite::RegexpLookbehind => DEFAULT_REGEXP_LOOKBEHIND_SUITE_PATH,
    Test262Suite::LanguageStatements => DEFAULT_LANGUAGE_STATEMENTS_SUITE_PATH,
    Test262Suite::LanguageFunctions => DEFAULT_LANGUAGE_FUNCTIONS_SUITE_PATH,
    Test262Suite::LanguageClasses => DEFAULT_LANGUAGE_CLASSES_SUITE_PATH,
    Test262Suite::LanguageScopes => DEFAULT_LANGUAGE_SCOPES_SUITE_PATH,
    Test262Suite::BuiltinsCore => DEFAULT_BUILTINS_CORE_SUITE_PATH,
    Test262Suite::BuiltinsJsonMath => DEFAULT_BUILTINS_JSON_MATH_SUITE_PATH,
  });
  if !suite_path.is_file() {
    bail!("suite file {} does not exist", suite_path.display());
  }

  let report_path = resolve_repo_path(&repo_root, &args.report);
  if let Some(parent) = report_path.parent() {
    fs::create_dir_all(parent)
      .with_context(|| format!("failed to create report directory {}", parent.display()))?;
  }

  let summary_path = resolve_repo_path(&repo_root, &args.summary);
  if let Some(parent) = summary_path.parent() {
    fs::create_dir_all(parent)
      .with_context(|| format!("failed to create summary directory {}", parent.display()))?;
  }

  let baseline_path = resolve_repo_path(&repo_root, &args.baseline);

  let jobs = crate::cpu_budget().min(DEFAULT_JOBS_CAP).max(1);
  let shard_arg = args.shard.map(|(idx, total)| format!("{idx}/{total}"));
  let fail_on_arg = match args.fail_on {
    FailOn::All => "all",
    FailOn::New => "new",
    FailOn::None => "none",
  };

  let mut cmd = xtask::cmd::cargo_agent_command(&repo_root);
  cmd
    .arg("run")
    .arg("--release")
    .args(["-p", "test262-semantic"])
    .arg("--")
    .arg("--test262-dir")
    .arg(&test262_dir)
    .arg("--harness")
    .arg(args.harness.as_cli_value())
    .arg("--suite-path")
    .arg(&suite_path)
    .arg("--manifest")
    .arg(&manifest_path)
    .arg("--timeout-secs")
    .arg(args.timeout_secs.to_string())
    .arg("--jobs")
    .arg(jobs.to_string())
    .arg("--report-path")
    .arg(&report_path)
    .arg("--fail-on")
    .arg(fail_on_arg);

  if let Some(shard) = shard_arg {
    cmd.arg("--shard").arg(shard);
  }

  if let Some(filter) = args.filter.as_ref() {
    cmd.arg("--filter").arg(filter);
  }
  if !args.extra.is_empty() {
    cmd.args(&args.extra);
  }

  cmd.current_dir(&ecma_rs_root);
  println!("Running test262 semantic suite ({:?})...", args.suite);

  crate::print_command(&cmd);
  let status = cmd
    .status()
    .with_context(|| format!("failed to run {:?}", cmd.get_program()))?;

  // The underlying runner should always write its report even on failures. Parse the report and
  // emit a human-readable summary so developers/CI have something actionable to look at.
  let report_exists = report_path.is_file();
  if !report_exists {
    bail!(
      "test262 runner did not write report JSON to {} (status={status})",
      report_path.display()
    );
  }

  let report = super::test262_report::read_report(&report_path)
    .with_context(|| format!("load test262 report {}", report_path.display()))?;
  let stats = super::test262_report::compute_report_stats(&report);

  let baseline_compare_enabled = matches!(args.suite, Test262Suite::Curated)
    && args.filter.is_none()
    && args.shard.is_none()
    && matches!(args.harness, HarnessMode::Test262)
    && baseline_path.is_file();

  let (baseline, comparison) = if baseline_compare_enabled {
    let baseline = super::test262_report::read_baseline(&baseline_path)
      .with_context(|| format!("load test262 baseline {}", baseline_path.display()))?;
    let comparison =
      super::test262_report::compare_to_baseline(&baseline, &report).with_context(|| {
        format!(
          "compare report {} against baseline {}",
          report_path.display(),
          baseline_path.display()
        )
      })?;
    (Some(baseline), Some(super::test262_report::take_comparison(comparison)))
  } else {
    (None, None)
  };

  let markdown = super::test262_report::render_markdown(
    &report,
    &stats,
    baseline.as_ref(),
    comparison.as_ref(),
    super::test262_report::MarkdownOptions {
      title: "test262 semantic report",
      report_path: &report_path,
      baseline_path: if baseline_compare_enabled {
        Some(&baseline_path)
      } else {
        None
      },
    },
  );
  super::test262_report::write_markdown(&summary_path, &markdown)
    .with_context(|| format!("write test262 summary {}", summary_path.display()))?;

  println!("JSON report: {}", report_path.display());
  println!("Summary: {}", summary_path.display());

  if args.update_baseline {
    if !matches!(args.suite, Test262Suite::Curated) || args.filter.is_some() || args.shard.is_some()
    {
      bail!("--update-baseline requires the full curated suite (no --filter/--shard)");
    }
    if !matches!(args.harness, HarnessMode::Test262) {
      bail!("--update-baseline requires --harness test262");
    }

    let baseline = super::test262_report::baseline_from_report(&report)?;
    super::test262_report::write_baseline(&baseline_path, &baseline)
      .with_context(|| format!("write baseline {}", baseline_path.display()))?;

    let baseline_dir = baseline_path
      .parent()
      .map(ToOwned::to_owned)
      .unwrap_or_else(|| repo_root.clone());
    let baseline_summary_path = baseline_dir.join("summary.md");
    let baseline_trend_path = baseline_dir.join("trend.json");

    // The committed baseline summary should describe the baseline itself (not a diff against the
    // previous baseline we may have loaded above).
    let baseline_markdown = super::test262_report::render_markdown(
      &report,
      &stats,
      None,
      None,
      super::test262_report::MarkdownOptions {
        title: "test262 semantic baseline",
        report_path: &baseline_path,
        baseline_path: None,
      },
    );
    super::test262_report::write_markdown(&baseline_summary_path, &baseline_markdown)
      .with_context(|| {
        format!(
          "write baseline summary {}",
          baseline_summary_path.display()
        )
      })?;

    let trend = super::test262_report::trend_from_report(&report);
    super::test262_report::write_trend(&baseline_trend_path, &trend).with_context(|| {
      format!(
        "write baseline trend file {}",
        baseline_trend_path.display()
      )
    })?;

    println!("Updated baseline: {}", baseline_path.display());
    println!("Baseline summary: {}", baseline_summary_path.display());
    println!("Baseline trend: {}", baseline_trend_path.display());

    if !status.success() {
      eprintln!(
        "Warning: test262 runner exited with non-zero status {status}, but baseline was updated because --update-baseline was specified."
      );
    }

    return Ok(());
  }

  // Optional environment-variable escape hatch (useful for local shell aliases).
  let gate_disabled_by_env = std::env::var_os("FASTR_TEST262_NO_GATE").is_some();
  let gate_enabled = baseline_compare_enabled && !args.no_gate && !gate_disabled_by_env;

  let mut gate_errors: Vec<String> = Vec::new();
  if gate_enabled {
    let comparison = comparison.as_ref().expect("baseline_compare_enabled implies comparison");

    let mut current_by_key = std::collections::BTreeMap::new();
    for result in &report.results {
      let key = super::test262_report::ResultKey::from_result(result).to_string_key();
      current_by_key.insert(key, result);
    }

    let mut unexpected_regressions = 0usize;
    for change in &comparison.regressions {
      let current = current_by_key
        .get(&change.key.to_string_key())
        .copied()
        .unwrap_or_else(|| {
          panic!(
            "comparison key {} missing from current report index",
            change.key
          )
        });
      if current.expectation.expectation == conformance_harness::ExpectationKind::Pass {
        unexpected_regressions += 1;
      }
    }

    if unexpected_regressions > 0 {
      gate_errors.push(format!(
        "{unexpected_regressions} unexpected regression(s) vs baseline (matched -> mismatched without a manifest skip/xfail/flaky)"
      ));
    }

    if !comparison.new_timeouts.is_empty() {
      gate_errors.push(format!(
        "{} new timeout(s) vs baseline (hangs must be fixed or skipped)",
        comparison.new_timeouts.len()
      ));
    }
  }

  let mut errors: Vec<String> = Vec::new();
  if !status.success() {
    errors.push(format!("test262 runner exited with status {status}"));
  }
  errors.extend(gate_errors);

  if errors.is_empty() {
    return Ok(());
  }

  bail!(
    "test262 run did not satisfy requested gates:\n  - {}\n\nSee summary: {}",
    errors.join("\n  - "),
    summary_path.display()
  );
}

fn resolve_repo_path(repo_root: &Path, path: &Path) -> PathBuf {
  if path.is_absolute() {
    path.to_path_buf()
  } else {
    repo_root.join(path)
  }
}

fn ensure_test262_dir(
  repo_root: &Path,
  test262_dir: &Path,
  harness_mode: HarnessMode,
) -> Result<()> {
  let test_dir = test262_dir.join("test");
  let harness_dir = test262_dir.join("harness");
  let harness_required = !matches!(harness_mode, HarnessMode::None);
  if test_dir.is_dir() && (!harness_required || harness_dir.is_dir()) {
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

  if harness_required {
    bail!(
      "test262 checkout directory {} is missing required folders (expected {}/test and {}/harness)",
      test262_dir.display(),
      test262_dir.display(),
      test262_dir.display()
    );
  }

  bail!(
    "test262 checkout directory {} is missing required folder (expected {}/test)",
    test262_dir.display(),
    test262_dir.display()
  );
}
