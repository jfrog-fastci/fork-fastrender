#![allow(dead_code)]

use clap::{ArgAction, Parser};
use fastrender::api::{FastRenderConfig, FastRenderFactory, FastRenderPoolConfig};
use fastrender::error::{Error, ResourceError};
use fastrender::resource::{
  CachingFetcher, FetchRequest, FetchedResource, HttpFetcher, HttpRequest, ResourceFetcher,
  ResourcePolicy,
};
use fastrender::text::font_db::FontConfig;
use fastrender::ui::cancel::CancelGens;
use fastrender::ui::messages::{
  KeyAction, NavigationReason, PointerButton, PointerModifiers, RepaintReason, TabId, UiToWorker,
  WorkerToUi,
};
use fastrender::ui::WorkerToUiInbox;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::mpsc::Sender;
use std::sync::Arc;
use std::time::{Duration, Instant};
use url::Url;

const UI_PERF_SMOKE_SCHEMA_VERSION: u32 = 1;
const RAYON_NUM_THREADS_ENV: &str = "RAYON_NUM_THREADS";

const DEFAULT_OUTPUT_PATH: &str = "target/ui_perf_smoke.json";

const DEFAULT_VIEWPORT_CSS: (u32, u32) = (800, 600);
const DEFAULT_DPR: f32 = 1.0;

const DEFAULT_THRESHOLD: f64 = 0.05;

const ACTION_TIMEOUT: Duration = Duration::from_secs(60);

const DEFAULT_WARMUP: usize = 1;

const SCROLL_WARMUP: usize = 5;
const SCROLL_SAMPLES: usize = 40;
const SCROLL_DELTA_CSS: f32 = 140.0;

const RESIZE_WARMUP: usize = 3;
const RESIZE_SAMPLES: usize = 20;

const INPUT_WARMUP: usize = 3;
const INPUT_CYCLES: usize = 20;

const TAB_SWITCH_WARMUP: usize = 5;
const TAB_SWITCH_SAMPLES: usize = 40;

fn default_rayon_threads() -> usize {
  1
}

#[derive(Parser)]
#[command(about = "Headless browser UI responsiveness harness (scroll/resize/input latency)")]
struct Args {
  /// Write the JSON summary to this path (also printed to stdout).
  #[arg(long, default_value = DEFAULT_OUTPUT_PATH)]
  output: PathBuf,

  /// Number of Rayon worker threads to use for rendering work.
  ///
  /// When provided, sets `RAYON_NUM_THREADS` before spawning the UI worker thread. When omitted and
  /// `RAYON_NUM_THREADS` is not already set, this harness defaults to 1 for deterministic output
  /// (CI-friendly).
  #[arg(long, value_name = "N")]
  rayon_threads: Option<usize>,

  /// Allow network fetches (http/https) when running the harness locally.
  ///
  /// By default, `ui_perf_smoke` runs in a deterministic offline mode (http/https disabled) so it
  /// can be safely used in CI and local runs without accidental network fetches.
  #[arg(long, action = ArgAction::SetTrue, visible_alias = "http")]
  allow_network: bool,

  /// Optional baseline JSON to compare against.
  #[arg(long)]
  baseline: Option<PathBuf>,

  /// Relative regression threshold (0.05 = 5%).
  #[arg(long, default_value_t = DEFAULT_THRESHOLD)]
  threshold: f64,

  /// Exit with a non-zero status when any tracked metric regresses beyond `--threshold`.
  #[arg(long)]
  fail_on_regression: bool,

  /// Only run these scenarios (comma-separated).
  #[arg(long, value_delimiter = ',', alias = "scenario")]
  only: Option<Vec<String>>,

  /// Additional warmup iterations per scenario.
  ///
  /// Each scenario already performs a small built-in warmup to reduce noise; this flag adds extra
  /// warmup iterations when you want more stable p95 numbers.
  ///
  /// Warmup iterations are executed but excluded from reported metrics/statistics.
  #[arg(long, default_value_t = DEFAULT_WARMUP)]
  warmup: usize,

  /// Override the per-scenario default number of measured iterations.
  ///
  /// - `ttfp_newtab`: number of tab open+navigate measurements.
  /// - `scroll_fixture` / `resize_fixture`: number of scroll/resize actions measured.
  /// - `input_text`: number of insert+delete cycles measured (2 samples per cycle).
  /// - `tab_switch`: number of tab switch measurements (A→B and B→A are each one sample).
  #[arg(long)]
  iterations: Option<usize>,

  /// Run each scenario in its own fresh UI worker thread instance.
  ///
  /// This reduces cross-scenario cache effects but increases total runtime.
  #[arg(long, action = ArgAction::SetTrue)]
  isolate: bool,

  /// Disable per-scenario isolation (overrides `--isolate` and any future defaults).
  #[arg(long, action = ArgAction::SetTrue)]
  no_isolate: bool,

  /// Exit with a non-zero status when any scenario fails (status=error|timeout).
  ///
  /// Defaults to enabled when the `CI` environment variable is set, otherwise disabled.
  #[arg(long)]
  fail_on_failure: bool,

  /// Disable the default `CI` behavior that enables `--fail-on-failure`.
  #[arg(long = "no-fail-on-failure", conflicts_with = "fail_on_failure")]
  no_fail_on_failure: bool,

  /// Print more debug information to stderr.
  #[arg(long, action = ArgAction::SetTrue)]
  verbose: bool,
}

#[derive(Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
enum ScenarioStatus {
  Ok,
  Error,
  Timeout,
}

impl Default for ScenarioStatus {
  fn default() -> Self {
    Self::Ok
  }
}

#[derive(Clone, Serialize, Deserialize)]
struct ScenarioSummary {
  name: String,
  url: String,
  viewport_css: (u32, u32),
  dpr: f32,
  #[serde(default)]
  rss_bytes_start: Option<u64>,
  #[serde(default)]
  rss_bytes_end: Option<u64>,
  #[serde(default)]
  rss_bytes_peak: Option<u64>,
  /// Convenience alias for `rss_bytes_end` (RSS observed immediately after the scenario).
  #[serde(default)]
  rss_after_bytes: Option<u64>,
  #[serde(default)]
  status: ScenarioStatus,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  error: Option<String>,
  #[serde(default)]
  samples_ms: Vec<f64>,
  #[serde(default)]
  metrics_ms: BTreeMap<String, f64>,
}

#[derive(Clone, Serialize, Deserialize)]
struct ResourcePolicySummary {
  allow_http: bool,
  allow_https: bool,
  allow_file: bool,
  allow_data: bool,
}

impl Default for ResourcePolicySummary {
  fn default() -> Self {
    // Deterministic/offline by default: match the `ui_perf_smoke` default policy.
    Self {
      allow_http: false,
      allow_https: false,
      allow_file: true,
      allow_data: true,
    }
  }
}

impl From<&ResourcePolicy> for ResourcePolicySummary {
  fn from(policy: &ResourcePolicy) -> Self {
    Self {
      allow_http: policy.allowed_schemes.http,
      allow_https: policy.allowed_schemes.https,
      allow_file: policy.allowed_schemes.file,
      allow_data: policy.allowed_schemes.data,
    }
  }
}

#[derive(Clone, Serialize, Deserialize)]
struct RunConfig {
  #[serde(default = "default_rayon_threads")]
  rayon_threads: usize,
  #[serde(default)]
  rayon_threads_source: RayonThreadsSource,
  #[serde(default = "default_rayon_threads")]
  effective_rayon_threads: usize,
  #[serde(default)]
  warmup: usize,
  #[serde(default)]
  isolate: bool,
  #[serde(default)]
  allow_network: bool,
  #[serde(default)]
  resource_policy: ResourcePolicySummary,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  iterations: Option<usize>,
}

impl Default for RunConfig {
  fn default() -> Self {
    Self {
      rayon_threads: 1,
      rayon_threads_source: RayonThreadsSource::Unknown,
      effective_rayon_threads: 1,
      warmup: 0,
      isolate: false,
      allow_network: false,
      resource_policy: ResourcePolicySummary::default(),
      iterations: None,
    }
  }
}

#[derive(Clone, Serialize, Deserialize)]
struct UiPerfSmokeSummary {
  schema_version: u32,
  #[serde(default)]
  run_config: RunConfig,
  #[serde(default)]
  rss_start_bytes: Option<u64>,
  #[serde(default)]
  rss_after_warmup_bytes: Option<u64>,
  scenarios: Vec<ScenarioSummary>,
}

struct FrameInfo {
  viewport_css: (u32, u32),
  dpr: f32,
  scroll_css: (f32, f32),
  scroll_bounds_css: fastrender::scroll::ScrollBounds,
  scroll_content_css: (f32, f32),
}

#[derive(Clone)]
struct Regression {
  scenario: String,
  metric: String,
  baseline: f64,
  latest: f64,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
  let args = Args::parse();
  if args.threshold < 0.0 {
    return Err("--threshold must be non-negative".into());
  }
  if args.fail_on_regression && args.baseline.is_none() {
    return Err("--fail-on-regression requires --baseline".into());
  }
  if args.rayon_threads == Some(0) {
    return Err("--rayon-threads must be greater than 0".into());
  }
  if args.iterations.is_some_and(|n| n == 0) {
    return Err("--iterations must be positive".into());
  }

  let isolate_default = false;
  let isolate = if args.no_isolate {
    false
  } else {
    args.isolate || isolate_default
  };
  if isolate {
    eprintln!("ui_perf_smoke: isolate enabled; spawning a fresh worker per scenario");
  }

  if std::env::var_os("FASTR_USE_BUNDLED_FONTS").is_none() {
    std::env::set_var("FASTR_USE_BUNDLED_FONTS", "1");
  }
  let rayon_threads_decision = resolve_requested_rayon_threads(args.rayon_threads);
  let requested_rayon_threads = rayon_threads_decision.requested.unwrap_or(1).max(1);
  let effective_rayon_threads = apply_rayon_threads_config(rayon_threads_decision);

  let scenario_names = selected_scenarios(args.only.as_deref())?;

  let baseline = if let Some(path) = args.baseline.as_ref() {
    Some(read_summary(path)?)
  } else {
    None
  };
  if let Some(base) = baseline.as_ref() {
    if base.schema_version != UI_PERF_SMOKE_SCHEMA_VERSION {
      return Err(
        format!(
          "baseline schema_version {} does not match current schema_version {} (regenerate the baseline with the current ui_perf_smoke)",
          base.schema_version, UI_PERF_SMOKE_SCHEMA_VERSION
        )
        .into(),
      );
    }
  }

  let fail_on_failure = resolve_fail_on_failure(&args);

  let mut scenarios = Vec::new();
  let policy = resource_policy_for_allow_network(args.allow_network);
  let run_config = RunConfig {
    rayon_threads: requested_rayon_threads,
    rayon_threads_source: rayon_threads_decision.source,
    effective_rayon_threads,
    warmup: args.warmup,
    isolate,
    allow_network: args.allow_network,
    resource_policy: ResourcePolicySummary::from(&policy),
    iterations: args.iterations,
  };

  let factory = build_ui_worker_factory(policy)?;

  let rss_start_bytes = fastrender::memory::current_rss_bytes();
  let rss_after_warmup_bytes;

  if isolate {
    // Best-effort warmup: prime global caches once before running isolated per-scenario workers.
    let (tx, rx, join) =
      fastrender::ui::spawn_ui_worker_with_factory("fastr-ui-perf-smoke-warmup", factory.clone())?
        .split();
    run_warmup(&tx, &rx, args.verbose);

    drop(tx);
    let join_result = join_with_timeout(join, Duration::from_secs(5));
    if let Err(err) = join_result {
      eprintln!("Warning: failed to join UI worker thread: {err}");
    }

    rss_after_warmup_bytes = fastrender::memory::current_rss_bytes();

    for name in &scenario_names {
      let worker_name = format!("fastr-ui-perf-smoke-{name}");
      let (tx, rx, join) =
        fastrender::ui::spawn_ui_worker_with_factory(worker_name, factory.clone())?.split();
      let summary = run_named_scenario(name, &tx, &rx, &run_config, args.verbose);
      let failed = summary.status != ScenarioStatus::Ok;
      scenarios.push(summary);

      drop(tx);
      // Best-effort: don't hang indefinitely waiting for the worker thread to exit.
      let join_result = join_with_timeout(join, Duration::from_secs(5));
      if let Err(err) = join_result {
        eprintln!("Warning: failed to join UI worker thread: {err}");
      }

      if failed && fail_on_failure {
        break;
      }
    }
  } else {
    let (tx, rx, join) =
      fastrender::ui::spawn_ui_worker_with_factory("fastr-ui-perf-smoke-worker", factory.clone())?
        .split();
    run_warmup(&tx, &rx, args.verbose);
    rss_after_warmup_bytes = fastrender::memory::current_rss_bytes();
    for name in &scenario_names {
      let summary = run_named_scenario(name, &tx, &rx, &run_config, args.verbose);
      let failed = summary.status != ScenarioStatus::Ok;
      scenarios.push(summary);
      if failed && fail_on_failure {
        break;
      }
    }

    drop(tx);
    // Best-effort: don't hang indefinitely waiting for the worker thread to exit.
    let join_result = join_with_timeout(join, Duration::from_secs(5));
    if let Err(err) = join_result {
      eprintln!("Warning: failed to join UI worker thread: {err}");
    }
  }

  scenarios.sort_by(|a, b| a.name.cmp(&b.name));
  let summary = UiPerfSmokeSummary {
    schema_version: UI_PERF_SMOKE_SCHEMA_VERSION,
    run_config,
    rss_start_bytes,
    rss_after_warmup_bytes,
    scenarios: scenarios.clone(),
  };

  if let Some(parent) = args.output.parent() {
    if !parent.as_os_str().is_empty() {
      std::fs::create_dir_all(parent)?;
    }
  }

  let json = serde_json::to_string_pretty(&summary)?;
  std::fs::write(&args.output, &json)?;
  println!("{json}");

  let mut exit_code = 0;

  if fail_on_failure {
    let failures: Vec<&ScenarioSummary> = scenarios
      .iter()
      .filter(|s| s.status != ScenarioStatus::Ok)
      .collect();
    if !failures.is_empty() {
      eprintln!("Scenario failures ({}):", failures.len());
      for scenario in failures {
        let message = scenario.error.as_deref().unwrap_or("scenario failed");
        eprintln!(
          "  {:<16} {:<7} {}",
          scenario.name,
          format_status(scenario.status),
          message
        );
      }
      exit_code = 1;
    }
  }

  if let Some(base) = baseline.as_ref() {
    let regressions = find_regressions(&summary, base, args.threshold);
    if regressions.is_empty() {
      eprintln!(
        "No regressions detected vs baseline (threshold {:.1}%).",
        args.threshold * 100.0
      );
    } else {
      eprintln!(
        "Regressions detected vs baseline ({} over threshold {:.1}%):",
        regressions.len(),
        args.threshold * 100.0
      );
      for reg in &regressions {
        eprintln!(
          "  {:<16} {:<24} baseline={:.3} latest={:.3} ({:+.1}%)",
          reg.scenario,
          reg.metric,
          reg.baseline,
          reg.latest,
          reg.percent_delta() * 100.0
        );
      }
      if args.fail_on_regression {
        exit_code = 1;
      }
    }
  }

  if exit_code != 0 {
    std::process::exit(exit_code);
  }
  Ok(())
}

fn resource_policy_for_allow_network(allow_network: bool) -> ResourcePolicy {
  ResourcePolicy::default()
    .allow_http(allow_network)
    .allow_https(allow_network)
    .allow_file(true)
    .allow_data(true)
}

fn build_ui_worker_factory(policy: ResourcePolicy) -> fastrender::Result<FastRenderFactory> {
  let renderer_config = FastRenderConfig::default()
    .with_font_sources(FontConfig::bundled_only())
    .with_resource_policy(policy);

  // `about:` pages are trusted UI surfaces and are allowed to load shared chrome assets via
  // `chrome://...`. We mirror the production browser worker by installing a minimal chrome-aware
  // fetcher wrapper around the default HTTP/file fetcher.
  let base_fetcher: Arc<dyn ResourceFetcher> = if let Some(cache) = renderer_config.resource_cache {
    let policy = renderer_config.resource_policy.clone();
    Arc::new(
      CachingFetcher::with_config(HttpFetcher::new().with_policy(policy.clone()), cache)
        .with_policy(policy),
    )
  } else {
    Arc::new(HttpFetcher::new().with_policy(renderer_config.resource_policy.clone()))
  };

  let fetcher = Arc::new(AboutChromeFetcher::new(base_fetcher));

  FastRenderFactory::with_config(
    FastRenderPoolConfig::new()
      .with_renderer_config(renderer_config)
      .with_fetcher(fetcher),
  )
}

#[derive(Clone)]
struct AboutChromeFetcher {
  default: Arc<dyn ResourceFetcher>,
}

impl AboutChromeFetcher {
  fn new(default: Arc<dyn ResourceFetcher>) -> Self {
    Self { default }
  }

  fn is_allowed_chrome_request(&self, req: &FetchRequest<'_>) -> bool {
    if req
      .client_origin
      .is_some_and(|origin| origin.scheme().eq_ignore_ascii_case("about"))
    {
      return true;
    }

    // Some call sites may not carry `client_origin` but still provide a referrer URL. Treat an
    // `about:` referrer as sufficient to allow internal chrome assets.
    if let Some(referrer) = req.referrer_url {
      // Avoid allocations by using a cheap prefix check first.
      if referrer
        .trim_start()
        .get(..6)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("about:"))
      {
        return true;
      }
      if let Ok(parsed) = Url::parse(referrer) {
        if parsed.scheme().eq_ignore_ascii_case("about") {
          return true;
        }
      }
    }

    false
  }

  fn fetch_chrome(&self, req: FetchRequest<'_>) -> fastrender::Result<FetchedResource> {
    if !self.is_allowed_chrome_request(&req) {
      let origin = req
        .client_origin
        .map(|o| o.to_string())
        .unwrap_or_else(|| "<unknown>".to_string());
      return Err(Error::Resource(ResourceError::new(
        req.url,
        format!("blocked chrome:// subresource fetch from origin {origin}"),
      )));
    }

    let url = req.url.trim();
    let parsed = Url::parse(url).map_err(|err| {
      Error::Resource(ResourceError::new(
        url,
        format!("invalid chrome:// URL {url:?}: {err}"),
      ))
    })?;

    if !parsed.scheme().eq_ignore_ascii_case("chrome") {
      return Err(Error::Resource(ResourceError::new(
        url,
        format!("expected chrome:// URL, got scheme={}", parsed.scheme()),
      )));
    }

    let host = parsed.host_str().unwrap_or_default();
    let path = parsed.path();
    match (host, path) {
      ("styles", "/about.css") => Ok(FetchedResource::new(
        include_bytes!("../../assets/chrome/about.css").to_vec(),
        Some("text/css".to_string()),
      )),
      _ => Err(Error::Resource(ResourceError::new(
        url,
        format!("unknown chrome:// asset chrome://{host}{path}"),
      ))),
    }
  }
}

impl ResourceFetcher for AboutChromeFetcher {
  fn fetch(&self, url: &str) -> fastrender::Result<FetchedResource> {
    // Without request metadata we cannot safely determine whether this `chrome://` request was
    // initiated by an `about:` document, so fail closed.
    if url
      .trim_start()
      .get(..9)
      .is_some_and(|p| p.eq_ignore_ascii_case("chrome://"))
    {
      return Err(Error::Resource(ResourceError::new(
        url,
        "blocked chrome:// fetch without an initiating about: origin".to_string(),
      )));
    }
    self.default.fetch(url)
  }

  fn fetch_with_request(&self, req: FetchRequest<'_>) -> fastrender::Result<FetchedResource> {
    if req
      .url
      .trim_start()
      .get(..9)
      .is_some_and(|p| p.eq_ignore_ascii_case("chrome://"))
    {
      return self.fetch_chrome(req);
    }
    self.default.fetch_with_request(req)
  }

  fn fetch_with_request_and_validation(
    &self,
    req: FetchRequest<'_>,
    etag: Option<&str>,
    last_modified: Option<&str>,
  ) -> fastrender::Result<FetchedResource> {
    if req
      .url
      .trim_start()
      .get(..9)
      .is_some_and(|p| p.eq_ignore_ascii_case("chrome://"))
    {
      let _ = (etag, last_modified);
      return self.fetch_chrome(req);
    }
    self
      .default
      .fetch_with_request_and_validation(req, etag, last_modified)
  }

  fn request_header_value(&self, req: FetchRequest<'_>, header_name: &str) -> Option<String> {
    if req
      .url
      .trim_start()
      .get(..9)
      .is_some_and(|p| p.eq_ignore_ascii_case("chrome://"))
    {
      return None;
    }
    self.default.request_header_value(req, header_name)
  }

  fn cookie_header_value(&self, url: &str) -> Option<String> {
    if url
      .trim_start()
      .get(..9)
      .is_some_and(|p| p.eq_ignore_ascii_case("chrome://"))
    {
      return Some(String::new());
    }
    self.default.cookie_header_value(url)
  }

  fn store_cookie_from_document(&self, url: &str, cookie_string: &str) {
    if url
      .trim_start()
      .get(..9)
      .is_some_and(|p| p.eq_ignore_ascii_case("chrome://"))
    {
      return;
    }
    self.default.store_cookie_from_document(url, cookie_string);
  }

  fn fetch_http_request(&self, req: HttpRequest<'_>) -> fastrender::Result<FetchedResource> {
    if req
      .fetch
      .url
      .trim_start()
      .get(..9)
      .is_some_and(|p| p.eq_ignore_ascii_case("chrome://"))
    {
      // Only allow `GET`/`HEAD`-style chrome fetches, mirroring the `ResourceFetcher` default
      // behavior.
      if !req.method.eq_ignore_ascii_case("GET") && !req.method.eq_ignore_ascii_case("HEAD") {
        return Err(Error::Resource(ResourceError::new(
          req.fetch.url,
          "blocked non-GET chrome:// request".to_string(),
        )));
      }
      let mut res = self.fetch_chrome(req.fetch)?;
      if req.method.eq_ignore_ascii_case("HEAD") {
        res.bytes.clear();
      }
      return Ok(res);
    }
    self.default.fetch_http_request(req)
  }
}

fn resolve_fail_on_failure(args: &Args) -> bool {
  if args.no_fail_on_failure {
    return false;
  }
  if args.fail_on_failure {
    return true;
  }
  std::env::var_os("CI").is_some()
}

fn env_var_is_nonempty(key: &str) -> bool {
  std::env::var_os(key).is_some_and(|value| !value.is_empty())
}

fn parse_env_threads() -> Option<usize> {
  let raw = std::env::var(RAYON_NUM_THREADS_ENV).ok()?;
  let raw = raw.trim();
  if raw.is_empty() {
    return None;
  }
  raw.parse::<usize>().ok().filter(|v| *v > 0)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum RayonThreadsSource {
  Cli,
  Env,
  HarnessDefault,
  #[serde(other)]
  Unknown,
}

impl Default for RayonThreadsSource {
  fn default() -> Self {
    Self::Unknown
  }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct RayonThreadsDecision {
  requested: Option<usize>,
  source: RayonThreadsSource,
}

fn resolve_requested_rayon_threads(cli_value: Option<usize>) -> RayonThreadsDecision {
  if let Some(value) = cli_value {
    return RayonThreadsDecision {
      requested: Some(value.max(1)),
      source: RayonThreadsSource::Cli,
    };
  }

  if let Some(env_threads) = parse_env_threads() {
    return RayonThreadsDecision {
      requested: Some(env_threads.max(1)),
      source: RayonThreadsSource::Env,
    };
  }

  RayonThreadsDecision {
    requested: Some(1),
    source: RayonThreadsSource::HarnessDefault,
  }
}

fn apply_rayon_threads_config(decision: RayonThreadsDecision) -> usize {
  let requested = decision.requested.unwrap_or(1).max(1);

  // When explicitly requested via `--rayon-threads`, set the environment variable before spawning
  // any workers or rendering. If Rayon has already initialized its global pool, this will be
  // best-effort; we detect and warn below.
  let should_set_env = match decision.source {
    RayonThreadsSource::Cli => true,
    RayonThreadsSource::HarnessDefault | RayonThreadsSource::Unknown => {
      !env_var_is_nonempty(RAYON_NUM_THREADS_ENV)
    }
    RayonThreadsSource::Env => false,
  };
  if should_set_env {
    std::env::set_var(RAYON_NUM_THREADS_ENV, requested.to_string());
  }

  let outcome = init_rayon_global_pool_best_effort(requested);
  if decision.source == RayonThreadsSource::Cli
    && outcome.already_initialized
    && outcome.effective != requested
  {
    eprintln!(
      "warning: requested --rayon-threads {requested}, but Rayon global pool is already initialized with {} thread(s)",
      outcome.effective
    );
  }
  outcome.effective
}

#[derive(Clone, Copy, Debug)]
struct RayonInitOutcome {
  effective: usize,
  already_initialized: bool,
}

fn init_rayon_global_pool_best_effort(requested_threads: usize) -> RayonInitOutcome {
  let requested_threads = requested_threads.max(1);
  let mut threads = requested_threads;
  let mut attempted: Vec<usize> = Vec::new();

  loop {
    attempted.push(threads);
    match rayon::ThreadPoolBuilder::new()
      .num_threads(threads)
      .build_global()
    {
      Ok(()) => {
        let effective = current_rayon_threads_fallback();
        return RayonInitOutcome {
          effective,
          already_initialized: false,
        };
      }
      Err(err) => {
        if threads <= 1 {
          let already_initialized = std::panic::catch_unwind(|| rayon::current_num_threads()).ok();
          if let Some(effective) = already_initialized {
            return RayonInitOutcome {
              effective: effective.max(1),
              already_initialized: true,
            };
          }
          eprintln!(
            "warning: failed to initialize Rayon global pool after trying {attempted:?}: {err}"
          );
          return RayonInitOutcome {
            effective: 1,
            already_initialized: false,
          };
        }

        // If initialization fails due to OS thread-spawn limits, retry with a smaller pool.
        threads = (threads / 2).max(1);
      }
    }
  }
}

fn current_rayon_threads_fallback() -> usize {
  std::panic::catch_unwind(|| rayon::current_num_threads())
    .ok()
    .unwrap_or(1)
    .max(1)
}

fn selected_scenarios(only: Option<&[String]>) -> Result<Vec<String>, Box<dyn std::error::Error>> {
  let default = [
    "ttfp_newtab",
    "scroll_fixture",
    "resize_fixture",
    "input_text",
    "tab_switch",
  ];
  let all = [
    "ttfp_newtab",
    "scroll_fixture",
    "resize_fixture",
    "input_text",
    "tab_switch",
    // Not part of the default run: used by CLI integration tests to ensure network is disabled by
    // default (CI-safe).
    "network_denied",
  ];

  match only {
    None => Ok(default.iter().map(|s| s.to_string()).collect()),
    Some(list) => {
      let wanted: BTreeSet<String> = list
        .iter()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
      if wanted.is_empty() {
        return Err("--only was provided but no scenario names were specified".into());
      }
      for name in &wanted {
        if !all.contains(&name.as_str()) {
          return Err(
            format!(
              "unknown scenario {name:?}; valid values: {}",
              all.join(", ")
            )
            .into(),
          );
        }
      }
      Ok(wanted.into_iter().collect())
    }
  }
}

fn format_status(status: ScenarioStatus) -> &'static str {
  match status {
    ScenarioStatus::Ok => "ok",
    ScenarioStatus::Error => "error",
    ScenarioStatus::Timeout => "timeout",
  }
}

fn run_warmup(tx: &Sender<UiToWorker>, rx: &WorkerToUiInbox, verbose: bool) {
  // Best-effort warmup: load a minimal internal page so the UI worker initializes fonts/caches
  // before we start measuring scenarios. Failures here should not abort the run.
  let tab_id = TabId::new();
  let url = fastrender::ui::about_pages::ABOUT_NEWTAB.to_string();
  if let Err(err) = create_and_navigate_tab(tx, tab_id, DEFAULT_VIEWPORT_CSS, DEFAULT_DPR, &url) {
    eprintln!("Warmup: failed to create/navigate tab: {err}");
    return;
  }

  match wait_for_frame(rx, tab_id, ACTION_TIMEOUT) {
    Ok(_) => {
      if verbose {
        eprintln!("Warmup: FrameReady received");
      }
    }
    Err(err) => {
      eprintln!("Warmup: {}", err.message);
    }
  }

  let _ = tx.send(UiToWorker::CloseTab { tab_id });
}

#[derive(Debug)]
struct RssPeakSampler {
  stop: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
  peak: Option<std::sync::Arc<std::sync::atomic::AtomicU64>>,
  has_sample: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
  join: Option<std::thread::JoinHandle<()>>,
}

impl RssPeakSampler {
  const SAMPLE_INTERVAL: Duration = Duration::from_millis(200);

  fn new(initial: Option<u64>) -> Self {
    let Some(initial) = initial else {
      return Self {
        stop: None,
        peak: None,
        has_sample: None,
        join: None,
      };
    };

    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

    let stop = std::sync::Arc::new(AtomicBool::new(false));
    let peak = std::sync::Arc::new(AtomicU64::new(initial));
    let has_sample = std::sync::Arc::new(AtomicBool::new(true));

    let stop_thread = stop.clone();
    let peak_thread = peak.clone();
    let has_sample_thread = has_sample.clone();
    let interval = Self::SAMPLE_INTERVAL;

    let join = std::thread::Builder::new()
      .name("ui-perf-smoke-rss".to_string())
      .spawn(move || {
        while !stop_thread.load(Ordering::Relaxed) {
          if let Some(rss_bytes) = fastrender::memory::current_rss_bytes() {
            has_sample_thread.store(true, Ordering::Relaxed);
            let mut prev = peak_thread.load(Ordering::Relaxed);
            while rss_bytes > prev {
              match peak_thread.compare_exchange(
                prev,
                rss_bytes,
                Ordering::Relaxed,
                Ordering::Relaxed,
              ) {
                Ok(_) => break,
                Err(next) => prev = next,
              }
            }
          }
          std::thread::sleep(interval);
        }
      })
      .ok();

    Self {
      stop: Some(stop),
      peak: Some(peak),
      has_sample: Some(has_sample),
      join,
    }
  }

  fn finish(mut self) -> Option<u64> {
    use std::sync::atomic::Ordering;

    if let Some(stop) = &self.stop {
      stop.store(true, Ordering::Relaxed);
    }

    if let Some(join) = self.join.take() {
      let _ = join.join();
    }

    match (&self.has_sample, &self.peak) {
      (Some(has_sample), Some(peak)) if has_sample.load(Ordering::Relaxed) => {
        Some(peak.load(Ordering::Relaxed))
      }
      _ => None,
    }
  }
}

fn run_named_scenario(
  name: &str,
  tx: &Sender<UiToWorker>,
  rx: &WorkerToUiInbox,
  run_config: &RunConfig,
  verbose: bool,
) -> ScenarioSummary {
  fn max_opt(a: Option<u64>, b: Option<u64>) -> Option<u64> {
    match (a, b) {
      (Some(a), Some(b)) => Some(a.max(b)),
      (Some(a), None) => Some(a),
      (None, Some(b)) => Some(b),
      (None, None) => None,
    }
  }

  let rss_bytes_start = fastrender::memory::current_rss_bytes();
  let rss_sampler = RssPeakSampler::new(rss_bytes_start);

  let mut summary = match name {
    "ttfp_newtab" => run_ttfp_newtab(tx, rx, run_config, verbose),
    "scroll_fixture" => run_scroll_fixture(tx, rx, run_config, verbose),
    "resize_fixture" => run_resize_fixture(tx, rx, run_config, verbose),
    "input_text" => run_input_text_fixture(tx, rx, run_config, verbose),
    "tab_switch" => run_tab_switch(tx, rx, run_config, verbose),
    "network_denied" => run_network_denied(tx, rx, run_config),
    other => ScenarioSummary {
      name: other.to_string(),
      url: String::new(),
      viewport_css: DEFAULT_VIEWPORT_CSS,
      dpr: DEFAULT_DPR,
      rss_bytes_start: None,
      rss_bytes_end: None,
      rss_bytes_peak: None,
      rss_after_bytes: None,
      status: ScenarioStatus::Error,
      error: Some(format!("unsupported scenario {other:?}")),
      samples_ms: Vec::new(),
      metrics_ms: BTreeMap::new(),
    },
  };

  let rss_bytes_end = fastrender::memory::current_rss_bytes();
  let rss_bytes_peak = max_opt(
    max_opt(rss_bytes_start, rss_bytes_end),
    rss_sampler.finish(),
  );
  summary.rss_bytes_start = rss_bytes_start;
  summary.rss_bytes_end = rss_bytes_end;
  summary.rss_bytes_peak = rss_bytes_peak;
  summary.rss_after_bytes = rss_bytes_end;
  summary
}

fn run_ttfp_newtab(
  tx: &Sender<UiToWorker>,
  rx: &WorkerToUiInbox,
  run_config: &RunConfig,
  verbose: bool,
) -> ScenarioSummary {
  let url = fastrender::ui::about_pages::ABOUT_NEWTAB.to_string();
  let viewport_css = DEFAULT_VIEWPORT_CSS;
  let dpr = DEFAULT_DPR;
  let warmup = run_config.warmup;
  let iterations = run_config.iterations.unwrap_or(1);

  let mut summary = ScenarioSummary {
    name: "ttfp_newtab".to_string(),
    url: url.clone(),
    viewport_css,
    dpr,
    rss_bytes_start: None,
    rss_bytes_end: None,
    rss_bytes_peak: None,
    rss_after_bytes: None,
    status: ScenarioStatus::Ok,
    error: None,
    samples_ms: Vec::new(),
    metrics_ms: BTreeMap::new(),
  };

  let mut measured = Vec::new();
  for i in 0..(warmup + iterations) {
    let tab_id = TabId::new();
    if let Err(err) = create_and_navigate_tab(tx, tab_id, viewport_css, dpr, &url) {
      summary.status = ScenarioStatus::Error;
      summary.error = Some(err.to_string());
      break;
    }

    let start = Instant::now();
    match wait_for_frame(rx, tab_id, ACTION_TIMEOUT) {
      Ok(_frame) => {
        let ttfp_ms = round_ms(start.elapsed().as_secs_f64() * 1000.0);
        if i >= warmup {
          measured.push(ttfp_ms);
          if verbose {
            eprintln!("ttfp_newtab: {:.3} ms", ttfp_ms);
          }
        }
      }
      Err(err) => {
        summary.status = err.status;
        summary.error = Some(err.message);
      }
    }

    let _ = tx.send(UiToWorker::CloseTab { tab_id });
    if summary.status != ScenarioStatus::Ok {
      break;
    }
  }

  if summary.status == ScenarioStatus::Ok {
    summary.samples_ms = measured.clone();
    let mut metrics = latency_metrics("ttfp", &measured);
    if let Some(p50) = metrics.get("ttfp_p50_ms").copied() {
      metrics.insert("ttfp_ms".to_string(), p50);
    }
    summary.metrics_ms = metrics;
  }
  summary
}

fn run_scroll_fixture(
  tx: &Sender<UiToWorker>,
  rx: &WorkerToUiInbox,
  run_config: &RunConfig,
  verbose: bool,
) -> ScenarioSummary {
  // Use the built-in layout stress fixture so scroll measurements reflect a non-trivial layout tree
  // (useful for responsiveness regressions).
  let url = fastrender::ui::about_pages::ABOUT_TEST_LAYOUT_STRESS.to_string();

  let viewport_css = DEFAULT_VIEWPORT_CSS;
  let dpr = DEFAULT_DPR;
  let tab_id = TabId::new();

  let mut summary = ScenarioSummary {
    name: "scroll_fixture".to_string(),
    url: url.clone(),
    viewport_css,
    dpr,
    rss_bytes_start: None,
    rss_bytes_end: None,
    rss_bytes_peak: None,
    rss_after_bytes: None,
    status: ScenarioStatus::Ok,
    error: None,
    samples_ms: Vec::new(),
    metrics_ms: BTreeMap::new(),
  };

  if let Err(err) = create_and_navigate_tab(tx, tab_id, viewport_css, dpr, &url) {
    summary.status = ScenarioStatus::Error;
    summary.error = Some(err.to_string());
    return summary;
  }

  let mut frame = match wait_for_frame(rx, tab_id, ACTION_TIMEOUT) {
    Ok(frame) => frame,
    Err(err) => {
      summary.status = err.status;
      summary.error = Some(err.message);
      let _ = tx.send(UiToWorker::CloseTab { tab_id });
      return summary;
    }
  };

  if frame.scroll_bounds_css.max_y <= frame.scroll_bounds_css.min_y + 1.0 {
    summary.status = ScenarioStatus::Error;
    summary.error = Some("fixture did not produce a scrollable document".to_string());
    let _ = tx.send(UiToWorker::CloseTab { tab_id });
    return summary;
  }

  let mut scroll_y = frame.scroll_css.1;
  let mut bounds = frame.scroll_bounds_css;
  let mut direction: f32 = 1.0;

  let warmup = SCROLL_WARMUP + run_config.warmup;
  let samples = run_config.iterations.unwrap_or(SCROLL_SAMPLES);

  let measured = match collect_measured_samples(warmup, samples, || -> Result<f64, WaitError> {
    loop {
      let mut target = scroll_y + direction * SCROLL_DELTA_CSS;
      if target > bounds.max_y {
        target = bounds.max_y;
      }
      if target < bounds.min_y {
        target = bounds.min_y;
      }
      if (target - scroll_y).abs() < 0.5 {
        direction *= -1.0;
        continue;
      }

      let start = Instant::now();
      tx.send(UiToWorker::ScrollTo {
        tab_id,
        pos_css: (0.0, target),
      })
      .map_err(|err| WaitError {
        status: ScenarioStatus::Error,
        message: format!("failed to send ScrollTo: {err}"),
      })?;

      let next = wait_for_frame(rx, tab_id, ACTION_TIMEOUT)?;
      frame = next;
      scroll_y = frame.scroll_css.1;
      bounds = frame.scroll_bounds_css;
      return Ok::<f64, WaitError>(round_ms(start.elapsed().as_secs_f64() * 1000.0));
    }
  }) {
    Ok(measured) => measured,
    Err(err) => {
      summary.status = err.status;
      summary.error = Some(err.message);
      Vec::new()
    }
  };

  if summary.status == ScenarioStatus::Ok {
    summary.samples_ms = measured.clone();
    summary.metrics_ms = latency_metrics("scroll_latency", &measured);
    if verbose {
      let p50 = summary
        .metrics_ms
        .get("scroll_latency_p50_ms")
        .copied()
        .unwrap_or(0.0);
      let p95 = summary
        .metrics_ms
        .get("scroll_latency_p95_ms")
        .copied()
        .unwrap_or(0.0);
      eprintln!(
        "scroll_fixture: bounds_y=[{:.1},{:.1}] content_h={:.1} p50={:.3}ms p95={:.3}ms",
        frame.scroll_bounds_css.min_y,
        frame.scroll_bounds_css.max_y,
        frame.scroll_content_css.1,
        p50,
        p95
      );
    }
  }

  let _ = tx.send(UiToWorker::CloseTab { tab_id });
  summary
}

fn run_resize_fixture(
  tx: &Sender<UiToWorker>,
  rx: &WorkerToUiInbox,
  run_config: &RunConfig,
  verbose: bool,
) -> ScenarioSummary {
  // Use the built-in layout stress fixture so resize measurements include a meaningful reflow cost
  // (the fixture's layout changes with width due to an auto-fit grid + wrapping text).
  let url = fastrender::ui::about_pages::ABOUT_TEST_LAYOUT_STRESS.to_string();

  let dpr = DEFAULT_DPR;
  let tab_id = TabId::new();

  let mut summary = ScenarioSummary {
    name: "resize_fixture".to_string(),
    url: url.clone(),
    viewport_css: DEFAULT_VIEWPORT_CSS,
    dpr,
    rss_bytes_start: None,
    rss_bytes_end: None,
    rss_bytes_peak: None,
    rss_after_bytes: None,
    status: ScenarioStatus::Ok,
    error: None,
    samples_ms: Vec::new(),
    metrics_ms: BTreeMap::new(),
  };

  if let Err(err) = create_and_navigate_tab(tx, tab_id, DEFAULT_VIEWPORT_CSS, dpr, &url) {
    summary.status = ScenarioStatus::Error;
    summary.error = Some(err.to_string());
    return summary;
  }

  if let Err(err) = wait_for_frame(rx, tab_id, ACTION_TIMEOUT) {
    summary.status = err.status;
    summary.error = Some(err.message);
    let _ = tx.send(UiToWorker::CloseTab { tab_id });
    return summary;
  }

  // Pick widths that force a different number of columns in the layout-stress fixture's
  // `repeat(auto-fit, minmax(240px, 1fr))` grid so resize latency reflects meaningful reflow.
  let wide = DEFAULT_VIEWPORT_CSS;
  let narrow = (360, DEFAULT_VIEWPORT_CSS.1);
  let warmup = RESIZE_WARMUP + run_config.warmup;
  let samples = run_config.iterations.unwrap_or(RESIZE_SAMPLES);
  let mut step = 0usize;

  let measured = match collect_measured_samples(warmup, samples, || -> Result<f64, WaitError> {
    let idx = step;
    step += 1;

    let viewport = if idx % 2 == 0 { wide } else { narrow };
    let start = Instant::now();
    tx.send(UiToWorker::ViewportChanged {
      tab_id,
      viewport_css: viewport,
      dpr,
    })
    .map_err(|err| WaitError {
      status: ScenarioStatus::Error,
      message: format!("failed to send ViewportChanged: {err}"),
    })?;

    let frame = wait_for_frame(rx, tab_id, ACTION_TIMEOUT)?;
    let dt_ms = round_ms(start.elapsed().as_secs_f64() * 1000.0);
    if verbose && idx >= warmup {
      eprintln!(
        "resize_fixture: viewport_css={}x{} -> frame viewport_css={}x{} dt={:.3}ms",
        viewport.0, viewport.1, frame.viewport_css.0, frame.viewport_css.1, dt_ms
      );
    }
    Ok::<f64, WaitError>(dt_ms)
  }) {
    Ok(measured) => measured,
    Err(err) => {
      summary.status = err.status;
      summary.error = Some(err.message);
      Vec::new()
    }
  };

  if summary.status == ScenarioStatus::Ok {
    summary.viewport_css = narrow;
    summary.samples_ms = measured.clone();
    summary.metrics_ms = latency_metrics("resize_latency", &measured);
  }

  let _ = tx.send(UiToWorker::CloseTab { tab_id });
  summary
}

fn run_input_text_fixture(
  tx: &Sender<UiToWorker>,
  rx: &WorkerToUiInbox,
  run_config: &RunConfig,
  verbose: bool,
) -> ScenarioSummary {
  let fixture_path = Path::new("tests/pages/fixtures/ui_perf_smoke/index.html");
  let url = match file_url(fixture_path) {
    Ok(url) => url,
    Err(err) => {
      return ScenarioSummary {
        name: "input_text".to_string(),
        url: fixture_path.display().to_string(),
        viewport_css: DEFAULT_VIEWPORT_CSS,
        dpr: DEFAULT_DPR,
        rss_bytes_start: None,
        rss_bytes_end: None,
        rss_bytes_peak: None,
        rss_after_bytes: None,
        status: ScenarioStatus::Error,
        error: Some(err.to_string()),
        samples_ms: Vec::new(),
        metrics_ms: BTreeMap::new(),
      };
    }
  };

  let viewport_css = DEFAULT_VIEWPORT_CSS;
  let dpr = DEFAULT_DPR;
  let tab_id = TabId::new();

  let mut summary = ScenarioSummary {
    name: "input_text".to_string(),
    url: url.clone(),
    viewport_css,
    dpr,
    rss_bytes_start: None,
    rss_bytes_end: None,
    rss_bytes_peak: None,
    rss_after_bytes: None,
    status: ScenarioStatus::Ok,
    error: None,
    samples_ms: Vec::new(),
    metrics_ms: BTreeMap::new(),
  };

  if let Err(err) = create_and_navigate_tab(tx, tab_id, viewport_css, dpr, &url) {
    summary.status = ScenarioStatus::Error;
    summary.error = Some(err.to_string());
    return summary;
  }

  if let Err(err) = wait_for_frame(rx, tab_id, ACTION_TIMEOUT) {
    summary.status = err.status;
    summary.error = Some(err.message);
    let _ = tx.send(UiToWorker::CloseTab { tab_id });
    return summary;
  }

  // Ensure we are scrolled to the top so the input field is visible.
  let _ = tx.send(UiToWorker::ScrollTo {
    tab_id,
    pos_css: (0.0, 0.0),
  });
  let _ = wait_for_frame(rx, tab_id, ACTION_TIMEOUT);

  // Click the fixed input in the header at a deterministic coordinate.
  let input_pos_css = (32.0, 24.0);
  let modifiers = PointerModifiers::NONE;

  let _ = tx.send(UiToWorker::PointerMove {
    tab_id,
    pos_css: input_pos_css,
    button: PointerButton::None,
    modifiers,
  });
  let _ = tx.send(UiToWorker::PointerDown {
    tab_id,
    pos_css: input_pos_css,
    button: PointerButton::Primary,
    modifiers,
    click_count: 1,
  });
  let _ = tx.send(UiToWorker::PointerUp {
    tab_id,
    pos_css: input_pos_css,
    button: PointerButton::Primary,
    modifiers,
  });

  let _ = wait_for_frame(rx, tab_id, ACTION_TIMEOUT);

  let warmup_cycles = INPUT_WARMUP + run_config.warmup;
  let cycles = run_config.iterations.unwrap_or(INPUT_CYCLES);

  let mut measured = Vec::with_capacity(cycles * 2);

  // Warm up: insert + delete a character a few times.
  for _ in 0..warmup_cycles {
    let _ = tx.send(UiToWorker::TextInput {
      tab_id,
      text: "a".to_string(),
    });
    let _ = wait_for_frame(rx, tab_id, ACTION_TIMEOUT);
    let _ = tx.send(UiToWorker::KeyAction {
      tab_id,
      key: KeyAction::Backspace,
    });
    let _ = wait_for_frame(rx, tab_id, ACTION_TIMEOUT);
  }

  for cycle in 0..cycles {
    let start = Instant::now();
    if let Err(err) = tx.send(UiToWorker::TextInput {
      tab_id,
      text: "a".to_string(),
    }) {
      summary.status = ScenarioStatus::Error;
      summary.error = Some(format!("failed to send TextInput: {err}"));
      break;
    }
    match wait_for_frame(rx, tab_id, ACTION_TIMEOUT) {
      Ok(_frame) => {
        let dt_ms = round_ms(start.elapsed().as_secs_f64() * 1000.0);
        measured.push(dt_ms);
        if verbose {
          eprintln!("input_text: insert cycle={} dt={:.3}ms", cycle, dt_ms);
        }
      }
      Err(err) => {
        summary.status = err.status;
        summary.error = Some(err.message);
        break;
      }
    }

    let start = Instant::now();
    if let Err(err) = tx.send(UiToWorker::KeyAction {
      tab_id,
      key: KeyAction::Backspace,
    }) {
      summary.status = ScenarioStatus::Error;
      summary.error = Some(format!("failed to send Backspace: {err}"));
      break;
    }
    match wait_for_frame(rx, tab_id, ACTION_TIMEOUT) {
      Ok(_frame) => {
        let dt_ms = round_ms(start.elapsed().as_secs_f64() * 1000.0);
        measured.push(dt_ms);
        if verbose {
          eprintln!("input_text: delete cycle={} dt={:.3}ms", cycle, dt_ms);
        }
      }
      Err(err) => {
        summary.status = err.status;
        summary.error = Some(err.message);
        break;
      }
    }
  }

  if summary.status == ScenarioStatus::Ok {
    summary.samples_ms = measured.clone();
    summary.metrics_ms = latency_metrics("input_latency", &measured);
  }

  let _ = tx.send(UiToWorker::CloseTab { tab_id });
  summary
}

fn run_tab_switch(
  tx: &Sender<UiToWorker>,
  rx: &WorkerToUiInbox,
  run_config: &RunConfig,
  verbose: bool,
) -> ScenarioSummary {
  let viewport_css = DEFAULT_VIEWPORT_CSS;
  let dpr = DEFAULT_DPR;
  let tab_a = TabId::new();
  let tab_b = TabId::new();

  let tab_a_url =
    if fastrender::ui::about_pages::html_for_about_url("about:test-layout-stress").is_some() {
      "about:test-layout-stress".to_string()
    } else {
      fastrender::ui::about_pages::ABOUT_TEST_HEAVY.to_string()
    };
  let tab_b_url = fastrender::ui::about_pages::ABOUT_NEWTAB.to_string();

  let mut summary = ScenarioSummary {
    name: "tab_switch".to_string(),
    url: format!("{tab_a_url} | {tab_b_url}"),
    viewport_css,
    dpr,
    rss_bytes_start: None,
    rss_bytes_end: None,
    rss_bytes_peak: None,
    rss_after_bytes: None,
    status: ScenarioStatus::Ok,
    error: None,
    samples_ms: Vec::new(),
    metrics_ms: BTreeMap::new(),
  };

  // Create tabs sequentially and wait for their first frames so the sampling loop measures tab
  // switching + repainting instead of initial navigation warmup work.
  if let Err(err) = create_and_navigate_tab(tx, tab_a, viewport_css, dpr, &tab_a_url) {
    summary.status = ScenarioStatus::Error;
    summary.error = Some(err.to_string());
    return summary;
  }
  if let Err(err) = wait_for_frame(rx, tab_a, ACTION_TIMEOUT) {
    summary.status = err.status;
    summary.error = Some(err.message);
    let _ = tx.send(UiToWorker::CloseTab { tab_id: tab_a });
    return summary;
  }

  if let Err(err) = create_and_navigate_tab(tx, tab_b, viewport_css, dpr, &tab_b_url) {
    summary.status = ScenarioStatus::Error;
    summary.error = Some(err.to_string());
    let _ = tx.send(UiToWorker::CloseTab { tab_id: tab_a });
    return summary;
  }
  if let Err(err) = wait_for_frame(rx, tab_b, ACTION_TIMEOUT) {
    summary.status = err.status;
    summary.error = Some(err.message);
    let _ = tx.send(UiToWorker::CloseTab { tab_id: tab_a });
    let _ = tx.send(UiToWorker::CloseTab { tab_id: tab_b });
    return summary;
  }

  // Establish a deterministic starting point.
  let _ = tx.send(UiToWorker::SetActiveTab { tab_id: tab_a });

  let warmup = TAB_SWITCH_WARMUP + run_config.warmup;
  let samples = run_config.iterations.unwrap_or(TAB_SWITCH_SAMPLES);

  let mut active = tab_a;
  let mut measured = Vec::new();

  for i in 0..(warmup + samples) {
    let next = if active == tab_a { tab_b } else { tab_a };
    // Drop any queued messages from previous paints (including buffered frames from a batch)
    // so the next `wait_for_frame` observes the paint triggered by this tab switch.
    for _ in rx.try_iter() {}
    let start = Instant::now();

    if let Err(err) = tx.send(UiToWorker::SetActiveTab { tab_id: next }) {
      summary.status = ScenarioStatus::Error;
      summary.error = Some(format!("failed to send SetActiveTab: {err}"));
      break;
    }
    if let Err(err) = tx.send(UiToWorker::RequestRepaint {
      tab_id: next,
      reason: RepaintReason::Explicit,
    }) {
      summary.status = ScenarioStatus::Error;
      summary.error = Some(format!("failed to send RequestRepaint: {err}"));
      break;
    }

    match wait_for_frame(rx, next, ACTION_TIMEOUT) {
      Ok(_frame) => {
        let dt_ms = round_ms(start.elapsed().as_secs_f64() * 1000.0);
        if i >= warmup {
          measured.push(dt_ms);
        }
        if verbose {
          eprintln!("tab_switch: {} -> {} dt={:.3}ms", active.0, next.0, dt_ms);
        }
      }
      Err(err) => {
        summary.status = err.status;
        summary.error = Some(err.message);
        break;
      }
    }

    active = next;
  }

  if summary.status == ScenarioStatus::Ok {
    summary.samples_ms = measured.clone();
    summary.metrics_ms = latency_metrics("tab_switch_latency", &measured);
    let total_ms = measured.iter().sum::<f64>();
    summary.metrics_ms.insert(
      "tab_switch_latency_total_ms".to_string(),
      round_ms(total_ms),
    );
  }

  let _ = tx.send(UiToWorker::CloseTab { tab_id: tab_a });
  let _ = tx.send(UiToWorker::CloseTab { tab_id: tab_b });
  summary
}

fn run_network_denied(
  tx: &Sender<UiToWorker>,
  rx: &WorkerToUiInbox,
  run_config: &RunConfig,
) -> ScenarioSummary {
  let viewport_css = DEFAULT_VIEWPORT_CSS;
  let dpr = DEFAULT_DPR;
  let tab_id = TabId::new();
  let target_url = "https://example.com".to_string();

  let mut summary = ScenarioSummary {
    name: "network_denied".to_string(),
    url: target_url.clone(),
    viewport_css,
    dpr,
    rss_bytes_start: None,
    rss_bytes_end: None,
    rss_bytes_peak: None,
    rss_after_bytes: None,
    status: ScenarioStatus::Ok,
    error: None,
    samples_ms: Vec::new(),
    metrics_ms: BTreeMap::new(),
  };

  if let Err(err) = create_and_navigate_tab(tx, tab_id, viewport_css, dpr, &target_url) {
    summary.status = ScenarioStatus::Error;
    summary.error = Some(err.to_string());
    return summary;
  }

  let start = Instant::now();
  match wait_for_navigation_outcome(rx, tab_id, Duration::from_secs(10)) {
    Ok(NavigationOutcome::Failed { error }) => {
      // Under the default configuration (`--allow-network` not provided), we expect this navigation
      // to fail immediately with a policy error. We intentionally report the navigation error so
      // integration tests can assert it is controlled (no hang).
      summary.status = ScenarioStatus::Error;
      summary.error = Some(error);
    }
    Ok(NavigationOutcome::Committed { url }) => {
      let dt_ms = round_ms(start.elapsed().as_secs_f64() * 1000.0);
      summary.samples_ms = vec![dt_ms];
      summary
        .metrics_ms
        .insert("navigation_committed_ms".to_string(), dt_ms);
      if !run_config.allow_network {
        summary.status = ScenarioStatus::Error;
        summary.error = Some(format!(
          "expected navigation to be blocked by policy (offline default), but committed {url}"
        ));
      }
    }
    Err(err) => {
      summary.status = err.status;
      summary.error = Some(err.message);
    }
  }

  let _ = tx.send(UiToWorker::CloseTab { tab_id });
  summary
}

fn create_and_navigate_tab(
  tx: &Sender<UiToWorker>,
  tab_id: TabId,
  viewport_css: (u32, u32),
  dpr: f32,
  url: &str,
) -> Result<(), Box<dyn std::error::Error>> {
  tx.send(UiToWorker::CreateTab {
    tab_id,
    initial_url: None,
    cancel: CancelGens::new(),
  })?;
  tx.send(UiToWorker::ViewportChanged {
    tab_id,
    viewport_css,
    dpr,
  })?;
  tx.send(UiToWorker::SetActiveTab { tab_id })?;
  tx.send(UiToWorker::Navigate {
    tab_id,
    url: url.to_string(),
    reason: NavigationReason::TypedUrl,
  })?;
  Ok(())
}

struct WaitError {
  status: ScenarioStatus,
  message: String,
}

enum NavigationOutcome {
  Committed { url: String },
  Failed { error: String },
}

fn wait_for_navigation_outcome(
  rx: &WorkerToUiInbox,
  tab_id: TabId,
  timeout: Duration,
) -> Result<NavigationOutcome, WaitError> {
  let deadline = Instant::now() + timeout;
  loop {
    let remaining = deadline.saturating_duration_since(Instant::now());
    match rx.recv_timeout(remaining) {
      Ok(WorkerToUi::NavigationCommitted {
        tab_id: msg_tab,
        url,
        ..
      }) if msg_tab == tab_id => return Ok(NavigationOutcome::Committed { url }),
      Ok(WorkerToUi::NavigationFailed {
        tab_id: msg_tab,
        error,
        ..
      }) if msg_tab == tab_id => return Ok(NavigationOutcome::Failed { error }),
      Ok(_) => {}
      Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
        return Err(WaitError {
          status: ScenarioStatus::Timeout,
          message: format!("timed out after {timeout:?} waiting for navigation outcome"),
        });
      }
      Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
        return Err(WaitError {
          status: ScenarioStatus::Error,
          message: "UI worker disconnected before navigation outcome".to_string(),
        });
      }
    }
  }
}

fn wait_for_frame(
  rx: &WorkerToUiInbox,
  tab_id: TabId,
  timeout: Duration,
) -> Result<FrameInfo, WaitError> {
  let deadline = Instant::now() + timeout;
  loop {
    let remaining = deadline.saturating_duration_since(Instant::now());
    match rx.recv_timeout(remaining) {
      Ok(WorkerToUi::FrameReady {
        tab_id: msg_tab,
        frame,
      }) if msg_tab == tab_id => {
        let metrics = frame.scroll_metrics;
        return Ok(FrameInfo {
          viewport_css: frame.viewport_css,
          dpr: frame.dpr,
          scroll_css: metrics.scroll_css,
          scroll_bounds_css: metrics.bounds_css,
          scroll_content_css: metrics.content_css,
        });
      }
      Ok(_) => {}
      Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
        return Err(WaitError {
          status: ScenarioStatus::Timeout,
          message: format!("timed out after {timeout:?} waiting for FrameReady"),
        });
      }
      Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
        return Err(WaitError {
          status: ScenarioStatus::Error,
          message: "UI worker disconnected before FrameReady".to_string(),
        });
      }
    }
  }
}

fn collect_measured_samples<T, E, F>(
  warmup: usize,
  iterations: usize,
  mut measure: F,
) -> Result<Vec<T>, E>
where
  F: FnMut() -> Result<T, E>,
{
  for _ in 0..warmup {
    let _ = measure()?;
  }
  let mut out = Vec::with_capacity(iterations);
  for _ in 0..iterations {
    out.push(measure()?);
  }
  Ok(out)
}

fn latency_metrics(prefix: &str, samples: &[f64]) -> BTreeMap<String, f64> {
  let mut out = BTreeMap::new();
  if samples.is_empty() {
    return out;
  }
  let mut sorted = samples.to_vec();
  sorted.sort_by(|a, b| a.total_cmp(b));
  let p50 = percentile_sorted(&sorted, 0.50);
  let p95 = percentile_sorted(&sorted, 0.95);
  let max = *sorted.last().unwrap(); // fastrender-allow-unwrap
  out.insert(format!("{prefix}_p50_ms"), round_ms(p50));
  out.insert(format!("{prefix}_p95_ms"), round_ms(p95));
  out.insert(format!("{prefix}_max_ms"), round_ms(max));
  out
}

fn percentile_sorted(sorted: &[f64], p: f64) -> f64 {
  if sorted.is_empty() {
    return 0.0;
  }
  if sorted.len() == 1 {
    return sorted[0];
  }
  let clamped = p.clamp(0.0, 1.0);
  let idx = (clamped * ((sorted.len() - 1) as f64)).round() as usize;
  sorted[idx.min(sorted.len() - 1)]
}

fn read_summary(path: &Path) -> Result<UiPerfSmokeSummary, Box<dyn std::error::Error>> {
  let data = std::fs::read_to_string(path)?;
  Ok(serde_json::from_str(&data)?)
}

fn find_regressions(
  latest: &UiPerfSmokeSummary,
  baseline: &UiPerfSmokeSummary,
  threshold: f64,
) -> Vec<Regression> {
  const MIN_DELTA_MS: f64 = 1.0;
  let baseline_map = baseline
    .scenarios
    .iter()
    .map(|s| (s.name.as_str(), s))
    .collect::<BTreeMap<_, _>>();

  let mut regressions = Vec::new();
  for scenario in &latest.scenarios {
    let Some(base) = baseline_map.get(scenario.name.as_str()) else {
      continue;
    };
    if scenario.status != ScenarioStatus::Ok || base.status != ScenarioStatus::Ok {
      continue;
    }
    for (metric, latest_value) in &scenario.metrics_ms {
      let Some(base_value) = base.metrics_ms.get(metric) else {
        continue;
      };
      if *base_value <= 0.0 {
        continue;
      }
      let delta = latest_value - base_value;
      if delta > MIN_DELTA_MS && (delta / base_value) > threshold {
        regressions.push(Regression {
          scenario: scenario.name.clone(),
          metric: metric.clone(),
          baseline: *base_value,
          latest: *latest_value,
        });
      }
    }
  }

  regressions.sort_by(|a, b| {
    (a.scenario.as_str(), a.metric.as_str()).cmp(&(b.scenario.as_str(), b.metric.as_str()))
  });
  regressions
}

impl Regression {
  fn percent_delta(&self) -> f64 {
    (self.latest - self.baseline) / self.baseline
  }
}

fn round_ms(value: f64) -> f64 {
  let rounded = (value * 1000.0).round() / 1000.0;
  if rounded == 0.0 {
    0.0
  } else {
    rounded
  }
}

fn file_url(path: &Path) -> Result<String, Box<dyn std::error::Error>> {
  let absolute = if path.is_absolute() {
    path.to_path_buf()
  } else {
    let repo_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    repo_root.join(path)
  };
  Ok(
    Url::from_file_path(&absolute)
      .map_err(|_| format!("could not convert {} to a file:// URL", absolute.display()))?
      .to_string(),
  )
}

fn join_with_timeout(join: std::thread::JoinHandle<()>, timeout: Duration) -> Result<(), String> {
  let (done_tx, done_rx) = std::sync::mpsc::channel::<std::thread::Result<()>>();
  std::thread::spawn(move || {
    let _ = done_tx.send(join.join());
  });
  match done_rx.recv_timeout(timeout) {
    Ok(Ok(())) => Ok(()),
    Ok(Err(_)) => Err("UI worker thread panicked".to_string()),
    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => Err(format!(
      "timed out after {timeout:?} waiting for UI worker join"
    )),
    Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
      Err("UI worker join helper thread disconnected".to_string())
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use clap::Parser;

  #[test]
  fn json_includes_run_config() {
    let summary = UiPerfSmokeSummary {
      schema_version: UI_PERF_SMOKE_SCHEMA_VERSION,
      run_config: RunConfig {
        rayon_threads: 1,
        effective_rayon_threads: 1,
        ..RunConfig::default()
      },
      rss_start_bytes: None,
      rss_after_warmup_bytes: None,
      scenarios: Vec::new(),
    };

    let value = serde_json::to_value(&summary).expect("serialize JSON");
    assert_eq!(value["run_config"]["rayon_threads"].as_u64(), Some(1));
    assert_eq!(
      value["run_config"]["rayon_threads_source"].as_str(),
      Some("unknown")
    );
    assert_eq!(
      value["run_config"]["effective_rayon_threads"].as_u64(),
      Some(1)
    );
    assert_eq!(value["run_config"]["warmup"].as_u64(), Some(0));
    assert_eq!(value["run_config"]["isolate"].as_bool(), Some(false));
    assert_eq!(
      value["run_config"]["allow_network"].as_bool(),
      Some(false),
      "allow_network should default to false (offline by default)"
    );
  }

  #[test]
  fn parses_rayon_threads_flag() {
    let args = Args::try_parse_from(["ui_perf_smoke", "--rayon-threads", "1"]).expect("parse args");
    assert_eq!(args.rayon_threads, Some(1));
  }

  #[test]
  fn default_cli_disables_http_and_https_in_output_config() {
    let args = Args::parse_from(["ui_perf_smoke"]);
    let policy = resource_policy_for_allow_network(args.allow_network);
    let summary = UiPerfSmokeSummary {
      schema_version: UI_PERF_SMOKE_SCHEMA_VERSION,
      run_config: RunConfig {
        warmup: args.warmup,
        allow_network: args.allow_network,
        resource_policy: ResourcePolicySummary::from(&policy),
        iterations: args.iterations,
        ..RunConfig::default()
      },
      rss_start_bytes: None,
      rss_after_warmup_bytes: None,
      scenarios: Vec::new(),
    };

    let value = serde_json::to_value(&summary).expect("serialize JSON");
    assert_eq!(
      value["run_config"]["resource_policy"]["allow_http"].as_bool(),
      Some(false)
    );
    assert_eq!(
      value["run_config"]["resource_policy"]["allow_https"].as_bool(),
      Some(false)
    );
    assert_eq!(
      value["run_config"]["resource_policy"]["allow_file"].as_bool(),
      Some(true)
    );
    assert_eq!(
      value["run_config"]["resource_policy"]["allow_data"].as_bool(),
      Some(true)
    );
  }

  #[test]
  fn warmup_samples_are_excluded_from_latency_metrics() {
    // Warmup sample is an extreme outlier and should not influence the reported metrics.
    let samples = [1000.0, 10.0, 20.0, 30.0];
    let mut idx = 0usize;
    let measured = collect_measured_samples(1, 3, || {
      let v = samples[idx];
      idx += 1;
      Ok::<f64, ()>(v)
    })
    .expect("collect");

    assert_eq!(measured, vec![10.0, 20.0, 30.0]);
    let metrics = latency_metrics("test", &measured);
    assert_eq!(metrics.get("test_p50_ms").copied(), Some(20.0));
    assert_eq!(metrics.get("test_p95_ms").copied(), Some(30.0));
    assert_eq!(metrics.get("test_max_ms").copied(), Some(30.0));
  }

  #[test]
  fn warmup_zero_uses_all_samples() {
    let samples = [10.0, 20.0, 30.0];
    let mut idx = 0usize;
    let measured = collect_measured_samples(0, 3, || {
      let v = samples[idx];
      idx += 1;
      Ok::<f64, ()>(v)
    })
    .expect("collect");

    assert_eq!(measured, vec![10.0, 20.0, 30.0]);
  }

  #[test]
  fn scenario_summary_includes_nullable_rss_fields() {
    let scenario = ScenarioSummary {
      name: "scenario".to_string(),
      url: "about:blank".to_string(),
      viewport_css: DEFAULT_VIEWPORT_CSS,
      dpr: DEFAULT_DPR,
      rss_bytes_start: None,
      rss_bytes_end: None,
      rss_bytes_peak: None,
      rss_after_bytes: None,
      status: ScenarioStatus::Ok,
      error: None,
      samples_ms: Vec::new(),
      metrics_ms: BTreeMap::new(),
    };

    let summary = UiPerfSmokeSummary {
      schema_version: UI_PERF_SMOKE_SCHEMA_VERSION,
      run_config: RunConfig::default(),
      rss_start_bytes: None,
      rss_after_warmup_bytes: None,
      scenarios: vec![scenario],
    };

    let value = serde_json::to_value(&summary).expect("serialize JSON");
    let scenario_value = &value["scenarios"][0];
    assert!(scenario_value.get("rss_bytes_start").is_some());
    assert!(scenario_value["rss_bytes_start"].is_null());
    assert!(scenario_value.get("rss_bytes_end").is_some());
    assert!(scenario_value["rss_bytes_end"].is_null());
    assert!(scenario_value.get("rss_bytes_peak").is_some());
    assert!(scenario_value["rss_bytes_peak"].is_null());
    assert!(scenario_value.get("rss_after_bytes").is_some());
    assert!(scenario_value["rss_after_bytes"].is_null());
  }
}
