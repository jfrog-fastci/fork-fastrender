use crate::diagnostic_norm::{normalize_rust_diagnostics, sort_diagnostics, NormalizedDiagnostic};
use crate::runner::{HarnessFileSet, HarnessHost, TimeoutManager};
use crate::{build_filter, discover_conformance_tests, FailOn, Shard, ShardStrategy};
use anyhow::{anyhow, Context, Result};
use clap::Args;
use rayon::{prelude::*, ThreadPoolBuilder};
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};
use typecheck_ts::lib_support::{LibName, ScriptTarget};
use typecheck_ts::Program;

pub(crate) const STRICT_NATIVE_BASELINE_SCHEMA_VERSION: u32 = 1;
const DEFAULT_TIMEOUT_SECS: u64 = 20;

fn default_jobs() -> usize {
  std::thread::available_parallelism()
    .map(|count| count.get())
    .unwrap_or(1)
    .min(4)
}

fn es_lib_for_target(target: ScriptTarget) -> LibName {
  let name = match target {
    ScriptTarget::Es3 | ScriptTarget::Es5 => "es5",
    ScriptTarget::Es2015 => "es2015",
    ScriptTarget::Es2016 => "es2016",
    ScriptTarget::Es2017 => "es2017",
    ScriptTarget::Es2018 => "es2018",
    ScriptTarget::Es2019 => "es2019",
    ScriptTarget::Es2020 => "es2020",
    ScriptTarget::Es2021 => "es2021",
    ScriptTarget::Es2022 => "es2022",
    ScriptTarget::EsNext => "esnext",
  };
  LibName::parse(name).expect("known lib name")
}

#[derive(Debug, Clone, Args)]
pub struct StrictNativeArgs {
  /// Override the fixtures root directory (defaults to the built-in strict-native fixtures).
  #[arg(long)]
  pub root: Option<PathBuf>,

  /// Glob or regex to filter tests (matches `<root>/<test>` ids).
  #[arg(long)]
  pub filter: Option<String>,

  /// Run only a shard (zero-based): `i/n`
  #[arg(long)]
  pub shard: Option<String>,

  /// Sharding strategy (default: index)
  #[arg(long, value_enum, default_value_t = ShardStrategy::Index)]
  pub shard_strategy: ShardStrategy,

  /// Whether to regenerate baselines from the Rust checker output.
  #[arg(long)]
  pub update_baselines: bool,

  /// Emit JSON output on stdout (suppresses the human summary).
  #[arg(long)]
  pub json: bool,

  /// Timeout per test case (seconds).
  #[arg(long, default_value_t = DEFAULT_TIMEOUT_SECS)]
  pub timeout_secs: u64,

  /// When to fail the run on mismatches.
  #[arg(long, value_enum, default_value_t = FailOn::New)]
  pub fail_on: FailOn,

  /// Allow mismatches without failing the command.
  #[arg(long)]
  pub allow_mismatches: bool,

  /// Number of worker threads to use.
  #[arg(long, default_value_t = default_jobs())]
  pub jobs: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum CaseStatus {
  Matched,
  Mismatch,
  BaselineUpdated,
  BaselineMissing,
  RustFailed,
  Timeout,
}

impl CaseStatus {
  fn is_error(&self) -> bool {
    matches!(
      self,
      CaseStatus::BaselineMissing | CaseStatus::RustFailed | CaseStatus::Timeout
    )
  }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CaseReport {
  id: String,
  status: CaseStatus,
  duration_ms: u128,
  #[serde(skip_serializing_if = "Option::is_none")]
  error: Option<String>,
  #[serde(skip_serializing_if = "Option::is_none")]
  expected: Option<Vec<NormalizedDiagnostic>>,
  #[serde(skip_serializing_if = "Option::is_none")]
  actual: Option<Vec<NormalizedDiagnostic>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct Summary {
  total: usize,
  matched: usize,
  mismatched: usize,
  updated: usize,
  errors: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct JsonReport {
  suite: String,
  summary: Summary,
  results: Vec<CaseReport>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct StrictNativeBaseline {
  #[serde(default)]
  pub(crate) schema_version: u32,
  #[serde(default)]
  pub(crate) diagnostics: Vec<NormalizedDiagnostic>,
}

impl StrictNativeBaseline {
  fn canonicalize_for_baseline(&mut self) {
    self.schema_version = STRICT_NATIVE_BASELINE_SCHEMA_VERSION;
    sort_diagnostics(&mut self.diagnostics);
  }
}

pub fn run(args: StrictNativeArgs) -> Result<()> {
  let root = args
    .root
    .clone()
    .unwrap_or_else(|| Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/strict-native"));
  if !root.exists() {
    return Err(anyhow!(
      "strict-native fixtures directory does not exist: {}",
      root.display()
    ));
  }

  let suite_name = root
    .file_name()
    .map(|s| s.to_string_lossy().to_string())
    .unwrap_or_else(|| "strict-native".to_string());

  let baselines_root = Path::new(env!("CARGO_MANIFEST_DIR"))
    .join("baselines")
    .join("strict-native");
  if args.update_baselines {
    fs::create_dir_all(&baselines_root).with_context(|| {
      format!(
        "create strict-native baselines directory at {}",
        baselines_root.display()
      )
    })?;
  }

  let filter = build_filter(args.filter.as_deref()).map_err(|err| anyhow!(err.to_string()))?;
  let extensions = vec!["ts".to_string()];
  let mut tests = discover_conformance_tests(&root, &filter, &extensions)
    .map_err(|err| anyhow!(err.to_string()))?;
  if tests.is_empty() {
    return Err(anyhow!(
      "strict-native suite `{}` contains no tests under {}",
      suite_name,
      root.display()
    ));
  }

  let shard = match args.shard.as_deref() {
    Some(raw) => Some(Shard::parse(raw).map_err(|err| anyhow!(err.to_string()))?),
    None => None,
  };
  if let Some(shard) = shard {
    tests = match args.shard_strategy {
      ShardStrategy::Index => tests
        .into_iter()
        .enumerate()
        .filter(|(idx, _)| shard.includes(*idx))
        .map(|(_, case)| case)
        .collect(),
      ShardStrategy::Hash => tests
        .into_iter()
        .filter(|case| shard.includes_hash(&case.id))
        .collect(),
    };
  }

  let jobs = args.jobs.max(1);
  let pool = ThreadPoolBuilder::new()
    .num_threads(jobs)
    .build()
    .map_err(|err| anyhow!("create thread pool: {err}"))?;

  let timeout = Duration::from_secs(args.timeout_secs);
  let timeout_manager = TimeoutManager::new();

  let results: Vec<CaseReport> = pool.install(|| {
    tests
      .par_iter()
      .map(|test| run_case(test, &args, &baselines_root, &timeout_manager, timeout))
      .collect()
  });

  let summary = summarize(&results);
  if args.json {
    let stdout = std::io::stdout();
    let mut handle = stdout.lock();
    serde_json::to_writer_pretty(
      &mut handle,
      &JsonReport {
        suite: suite_name.clone(),
        summary: summary.clone(),
        results: results.clone(),
      },
    )
    .context("serialize JSON output")?;
    writeln!(handle).context("write JSON output")?;
  } else {
    print_human_summary(&suite_name, &summary, &results);
  }

  let mismatch_total = summary.mismatched + summary.errors;
  let should_fail = args.fail_on.should_fail(mismatch_total, mismatch_total);
  if !args.allow_mismatches && should_fail {
    return Err(anyhow!(
      "strict-native failures: {mismatch_total} mismatch/error(s) (mismatched={}, errors={})",
      summary.mismatched,
      summary.errors
    ));
  }

  Ok(())
}

fn summarize(results: &[CaseReport]) -> Summary {
  let mut summary = Summary::default();
  summary.total = results.len();
  for case in results {
    match case.status {
      CaseStatus::Matched => summary.matched += 1,
      CaseStatus::Mismatch => summary.mismatched += 1,
      CaseStatus::BaselineUpdated => summary.updated += 1,
      CaseStatus::BaselineMissing | CaseStatus::RustFailed | CaseStatus::Timeout => {
        summary.errors += 1
      }
    }
  }
  summary
}

fn print_human_summary(suite: &str, summary: &Summary, results: &[CaseReport]) {
  println!(
    "strict-native: suite `{suite}` — total={}, matched={}, mismatched={}, updated={}, errors={}",
    summary.total, summary.matched, summary.mismatched, summary.updated, summary.errors
  );

  if summary.mismatched == 0 && summary.errors == 0 {
    return;
  }

  for case in results.iter().filter(|c| c.status.is_error()) {
    if let Some(err) = &case.error {
      eprintln!("  {}: {:?} ({err})", case.id, case.status);
    } else {
      eprintln!("  {}: {:?}", case.id, case.status);
    }
  }

  for case in results.iter().filter(|c| c.status == CaseStatus::Mismatch) {
    eprintln!("  {}: mismatch", case.id);
  }
}

fn baseline_path_for(baselines_root: &Path, id: &str) -> PathBuf {
  let rel = Path::new(id);
  let mut path = baselines_root.join(rel);
  if let Some(name) = path.file_name().and_then(|s| s.to_str()) {
    path.set_file_name(format!("{name}.json"));
  } else {
    path.set_file_name("baseline.json");
  }
  path
}

fn read_baseline(path: &Path) -> Result<StrictNativeBaseline> {
  let raw = fs::read_to_string(path)
    .with_context(|| format!("read baseline file at {}", path.display()))?;
  let mut baseline: StrictNativeBaseline =
    serde_json::from_str(&raw).with_context(|| format!("parse baseline {}", path.display()))?;
  baseline.canonicalize_for_baseline();
  Ok(baseline)
}

fn write_baseline(path: &Path, diagnostics: Vec<NormalizedDiagnostic>) -> Result<()> {
  if let Some(parent) = path.parent() {
    fs::create_dir_all(parent)
      .with_context(|| format!("create baseline directory {}", parent.display()))?;
  }
  let mut baseline = StrictNativeBaseline {
    schema_version: STRICT_NATIVE_BASELINE_SCHEMA_VERSION,
    diagnostics,
  };
  baseline.canonicalize_for_baseline();

  let file =
    fs::File::create(path).with_context(|| format!("write baseline {}", path.display()))?;
  let mut writer = BufWriter::new(file);
  serde_json::to_writer_pretty(&mut writer, &baseline)
    .with_context(|| format!("serialize baseline {}", path.display()))?;
  writeln!(writer).with_context(|| format!("write baseline {}", path.display()))?;
  Ok(())
}

fn run_case(
  test: &crate::TestCase,
  args: &StrictNativeArgs,
  baselines_root: &Path,
  timeout_manager: &TimeoutManager,
  timeout: Duration,
) -> CaseReport {
  let started = Instant::now();
  let deadline = started + timeout;

  let baseline_path = baseline_path_for(baselines_root, &test.id);

  let file_set = HarnessFileSet::new(&test.files);
  let mut compiler_options = test.options.to_compiler_options();
  compiler_options.native_strict = true;
  // The default lib set includes `dom`, which is extremely expensive to load in
  // debug builds and isn't needed by this suite. Restrict to the ES lib implied
  // by the target unless the test explicitly specifies a `// @lib:` list.
  if compiler_options.libs.is_empty() && !compiler_options.no_default_lib {
    compiler_options.libs = vec![es_lib_for_target(compiler_options.target)];
    if matches!(compiler_options.target, ScriptTarget::EsNext) {
      compiler_options
        .libs
        .push(LibName::parse("esnext.disposable").expect("known lib name"));
    }
  }
  let host = HarnessHost::new(file_set.clone(), compiler_options, test.options.type_roots.clone())
    .with_base_url_and_paths(test.options.base_url.clone(), test.options.paths.clone());
  let roots = file_set.root_keys();
  let program = Arc::new(Program::new(host, roots));

  let timeout_guard = timeout_manager.register(deadline);
  timeout_guard.set_program(Arc::clone(&program));

  let rust = run_rust_check(&program, &file_set, timeout);
  let duration_ms = started.elapsed().as_millis();

  let mut notes: Option<String> = None;
  let status = match rust {
    RustOutcome::Timeout(err) => {
      notes = Some(err);
      CaseStatus::Timeout
    }
    RustOutcome::Failed(err) => {
      notes = Some(err);
      CaseStatus::RustFailed
    }
    RustOutcome::Ok(actual) => {
      if args.update_baselines {
        if let Err(err) = write_baseline(&baseline_path, actual.clone()) {
          notes = Some(err.to_string());
          CaseStatus::RustFailed
        } else {
          CaseStatus::BaselineUpdated
        }
      } else {
        let expected = match read_baseline(&baseline_path) {
          Ok(b) => b.diagnostics,
          Err(err) => {
            notes = Some(err.to_string());
            return CaseReport {
              id: test.id.clone(),
              status: CaseStatus::BaselineMissing,
              duration_ms,
              error: notes,
              expected: None,
              actual: Some(actual),
            };
          }
        };
        if expected == actual {
          return CaseReport {
            id: test.id.clone(),
            status: CaseStatus::Matched,
            duration_ms,
            error: None,
            expected: Some(expected),
            actual: Some(actual),
          };
        }
        return CaseReport {
          id: test.id.clone(),
          status: CaseStatus::Mismatch,
          duration_ms,
          error: None,
          expected: Some(expected),
          actual: Some(actual),
        };
      }
    }
  };

  CaseReport {
    id: test.id.clone(),
    status,
    duration_ms,
    error: notes,
    expected: None,
    actual: None,
  }
}

enum RustOutcome {
  Ok(Vec<NormalizedDiagnostic>),
  Timeout(String),
  Failed(String),
}

fn run_rust_check(program: &Program, file_set: &HarnessFileSet, timeout: Duration) -> RustOutcome {
  match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| program.check_fallible())) {
    Err(_) => RustOutcome::Failed("typechecker panicked".to_string()),
    Ok(Err(typecheck_ts::FatalError::Cancelled)) => {
      RustOutcome::Timeout(format!("timed out after {}ms", timeout.as_millis()))
    }
    Ok(Err(fatal)) => RustOutcome::Failed(fatal.to_string()),
    Ok(Ok(diags)) => {
      let mut normalized = normalize_rust_diagnostics(&diags, |id| {
        program
          .file_key(id)
          .and_then(|key| file_set.name_for_key(&key).map(|name| name.to_string()))
      });
      sort_diagnostics(&mut normalized);
      RustOutcome::Ok(normalized)
    }
  }
}
