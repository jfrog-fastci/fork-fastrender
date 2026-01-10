//! Fetch a single page and render it to an image.
//!
//! Examples:
//!   fetch_and_render --timeout 120 --viewport 1200x800 --dpr 2.0 https://www.example.com output.png

#![allow(clippy::io_other_error)]
#![allow(clippy::redundant_closure)]
#![allow(clippy::len_zero)]
#![allow(clippy::items_after_test_module)]

use fastrender::cli_utils as common;

use clap::{ArgAction, Parser};
use common::args::{
  AllowPartialArgs, AnimationTimeArgs, BaseUrlArgs, CompatArgs, DiskCacheArgs, JsExecutionArgs,
  LayoutParallelArgs, MediaArgs, MemoryGuardArgs, OutputFormatArgs, RenderParseArgs,
  ResourceAccessArgs, TimeoutArgs, ViewportArgs,
};
use common::media_prefs::MediaPreferences;
use common::render_pipeline::{
  build_http_fetcher, build_render_configs, compute_soft_timeout_ms,
  follow_client_redirects_resource, format_error_with_chain, log_diagnostics, read_cached_document,
  render_fetched_document, RenderConfigBundle, RenderSurface, CLI_RENDER_STACK_SIZE,
};
use encoding_rs::{Encoding, UTF_8};
use fastrender::api::{FastRenderPool, FastRenderPoolConfig};
use fastrender::dom2::Document as Dom2Document;
use fastrender::html::base_url_tracker::BaseUrlTracker;
use fastrender::html::streaming_parser::{StreamingHtmlParser, StreamingParserYield};
use fastrender::image_output::encode_image;
use fastrender::js::{
  CurrentScriptHost, DomHost, EventLoop, MicrotaskCheckpointLimitedOutcome, ModuleGraphLoader,
  RunLimits, RunNextTaskLimitedOutcome, RunState, RunUntilIdleOutcome, RunUntilIdleStopReason,
  ScriptId, ScriptOrchestrator, ScriptScheduler, ScriptSchedulerAction, ScriptType, TaskSource,
  WindowHostState,
};
use fastrender::js::streaming_dom2::build_parser_inserted_script_element_spec_dom2;
use fastrender::render_control::{DeadlineGuard, RenderDeadline};
use fastrender::resource::normalize_user_agent_for_log;
use fastrender::resource::url_to_filename;
#[cfg(not(feature = "disk_cache"))]
use fastrender::resource::CachingFetcher;
use fastrender::resource::CachingFetcherConfig;
#[cfg(feature = "disk_cache")]
use fastrender::resource::DiskCachingFetcher;
use fastrender::resource::FetchDestination;
use fastrender::resource::FetchRequest;
use fastrender::resource::ResourceFetcher;
use fastrender::resource::DEFAULT_ACCEPT_LANGUAGE;
use fastrender::resource::DEFAULT_USER_AGENT;
use fastrender::OutputFormat;
use fastrender::Result;
use std::error::Error;
use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::mpsc::channel;
use std::sync::mpsc::RecvTimeoutError;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};
use vm_js::Budget;
use selectors::context::QuirksMode;
const DEFAULT_ASSET_CACHE_DIR: &str = "fetches/assets";
const DEFAULT_JS_MAX_TASKS: usize = 1024;
const DEFAULT_JS_MAX_MICROTASKS: usize = 4096;
const DEFAULT_JS_MAX_WALL_MS: u64 = 200;
const DEFAULT_JS_MAX_SCRIPT_BYTES: usize = 256 * 1024;

#[derive(Debug, Clone, Copy)]
struct JsCliConfig {
  enabled: bool,
  max_tasks: usize,
  max_microtasks: usize,
  max_wall_ms: u64,
  max_script_bytes: usize,
}

impl JsCliConfig {
  fn run_limits(&self) -> RunLimits {
    RunLimits {
      max_tasks: self.max_tasks,
      max_microtasks: self.max_microtasks,
      max_wall_time: (self.max_wall_ms > 0).then(|| Duration::from_millis(self.max_wall_ms)),
    }
  }
}

const DEFAULT_JS_FUEL: u64 = 5_000_000;
const DEFAULT_JS_CHECK_TIME_EVERY: u32 = 100;

fn js_budget_for_script(run_limits: RunLimits) -> Budget {
  let render_remaining =
    fastrender::render_control::root_deadline().and_then(|deadline| deadline.remaining_timeout());

  let deadline_duration = match (run_limits.max_wall_time, render_remaining) {
    (Some(a), Some(b)) => Some(a.min(b)),
    (Some(a), None) => Some(a),
    (None, Some(b)) => Some(b),
    (None, None) => None,
  };
  let deadline = deadline_duration.and_then(|d| Instant::now().checked_add(d));

  Budget {
    fuel: Some(DEFAULT_JS_FUEL),
    deadline,
    check_time_every: DEFAULT_JS_CHECK_TIME_EVERY,
  }
}

// HTML defines "ASCII whitespace" as: U+0009 TAB, U+000A LF, U+000C FF, U+000D CR, U+0020 SPACE.
fn trim_ascii_whitespace(value: &str) -> &str {
  value.trim_matches(|c: char| matches!(c, '\u{0009}' | '\u{000A}' | '\u{000C}' | '\u{000D}' | ' '))
}

fn charset_from_content_type(content_type: &str) -> Option<&str> {
  for param in content_type.split(';').skip(1) {
    let mut parts = param.splitn(2, '=');
    let name = trim_ascii_whitespace(parts.next()?);
    if !name.eq_ignore_ascii_case("charset") {
      continue;
    }
    let value = trim_ascii_whitespace(parts.next()?);
    let value = value.trim_matches('"').trim_matches('\'');
    if !value.is_empty() {
      return Some(value);
    }
  }
  None
}

/// Decode an external classic script resource into UTF-8 source text.
///
/// Best-effort HTML-shaped decoding:
/// - BOM wins when present.
/// - `charset` attribute on the `<script>` element takes priority.
/// - Otherwise, honor HTTP `Content-Type` charset when present.
/// - Otherwise, default to UTF-8 (scripts do not use the HTML Windows-1252 fallback).
fn decode_external_classic_script_bytes(
  bytes: &[u8],
  content_type: Option<&str>,
  charset_attr: Option<&str>,
) -> String {
  if bytes.is_empty() {
    return String::new();
  }

  if let Some((enc, bom_len)) = Encoding::for_bom(bytes) {
    return enc
      .decode_without_bom_handling(&bytes[bom_len..])
      .0
      .into_owned();
  }

  if let Some(label) = charset_attr
    .map(trim_ascii_whitespace)
    .filter(|v| !v.is_empty())
  {
    let label = label.trim_matches('"').trim_matches('\'');
    if let Some(enc) = Encoding::for_label(label.as_bytes()) {
      return enc.decode_with_bom_removal(bytes).0.into_owned();
    }
  }

  if let Some(label) = content_type.and_then(charset_from_content_type) {
    if let Some(enc) = Encoding::for_label(label.as_bytes()) {
      return enc.decode_with_bom_removal(bytes).0.into_owned();
    }
  }

  UTF_8.decode_with_bom_removal(bytes).0.into_owned()
}

fn fetch_script_source(
  fetcher: &dyn ResourceFetcher,
  document_url: &str,
  url: &str,
  destination: FetchDestination,
  max_script_bytes: usize,
) -> Result<fastrender::resource::FetchedResource> {
  debug_assert!(
    matches!(
      destination,
      FetchDestination::Script | FetchDestination::ScriptCors
    ),
    "fetch_script_source should only be used for classic <script src> fetches"
  );
  let mut req = FetchRequest::new(url, destination);
  req = req.with_referrer_url(document_url);
  if max_script_bytes == usize::MAX {
    fetcher.fetch_with_request(req)
  } else {
    // Fetch at most `max_script_bytes + 1` so we can deterministically decide whether
    // the script exceeds the configured budget without downloading arbitrary amounts.
    fetcher.fetch_partial_with_request(req, max_script_bytes.saturating_add(1))
  }
}

/// Fetch a single page and render it to an image
#[derive(Parser, Debug)]
#[command(name = "fetch_and_render", version, about)]
struct Args {
  /// URL to fetch and render
  url: String,

  /// Output file path (defaults to <url>.<format>); parent directories are created automatically
  output: Option<String>,

  /// Viewport width (deprecated, use --viewport)
  #[arg(hide = true)]
  width: Option<u32>,

  /// Viewport height (deprecated, use --viewport)
  #[arg(hide = true)]
  height: Option<u32>,

  /// Scroll X offset (deprecated, use --scroll-x)
  #[arg(hide = true)]
  scroll_x_pos: Option<u32>,

  /// Scroll Y offset (deprecated, use --scroll-y)
  #[arg(hide = true)]
  scroll_y_pos: Option<u32>,

  #[command(flatten)]
  timeout: TimeoutArgs,

  /// Cooperative timeout handed to the renderer in milliseconds (0 disables)
  ///
  /// When unset, defaults to (timeout - buffer) to allow a graceful timeout before the hard kill.
  /// The buffer is 5% of the hard timeout, clamped to 250ms..10s.
  /// Ignored if --timeout is not set (or is 0).
  #[arg(long, value_name = "MS")]
  soft_timeout_ms: Option<u64>,

  #[command(flatten)]
  memory: MemoryGuardArgs,

  #[command(flatten)]
  surface: ViewportArgs,

  #[command(flatten)]
  animation_time: AnimationTimeArgs,

  /// Horizontal scroll offset in CSS px
  #[arg(long, default_value = "0")]
  scroll_x: u32,

  /// Vertical scroll offset in CSS px
  #[arg(long, default_value = "0")]
  scroll_y: u32,

  #[command(flatten)]
  media: MediaArgs,

  #[command(flatten)]
  output_format: OutputFormatArgs,

  #[command(flatten)]
  base_url: BaseUrlArgs,

  #[command(flatten)]
  allow_partial: AllowPartialArgs,

  #[command(flatten)]
  resource_access: ResourceAccessArgs,

  #[command(flatten)]
  compat: CompatArgs,

  #[command(flatten)]
  render_parse: RenderParseArgs,

  /// Enable JavaScript execution (experimental)
  #[arg(long = "js", action = ArgAction::SetTrue)]
  js_enabled: bool,

  #[command(flatten)]
  js: JsExecutionArgs,

  /// Disable serving fresh cached HTTP responses without revalidation
  #[arg(long, action = ArgAction::SetTrue)]
  no_http_freshness: bool,

  #[command(flatten)]
  disk_cache: DiskCacheArgs,

  /// Override disk cache directory (defaults to fetches/assets)
  ///
  /// Note: this only has an effect when the binary is built with the `disk_cache` cargo feature.
  #[arg(long, default_value = DEFAULT_ASSET_CACHE_DIR)]
  cache_dir: PathBuf,
  #[command(flatten)]
  layout_parallel: LayoutParallelArgs,

  /// Expand render target to full content size
  #[arg(long)]
  full_page: bool,

  /// Override the User-Agent header
  #[arg(long, default_value = DEFAULT_USER_AGENT)]
  user_agent: String,

  /// Override the Accept-Language header
  #[arg(long, default_value = DEFAULT_ACCEPT_LANGUAGE)]
  accept_language: String,

  /// Maximum number of external stylesheets to fetch
  #[arg(long)]
  css_limit: Option<usize>,

  /// Enable per-stage timing logs
  #[arg(long)]
  timings: bool,

  /// Write a Chrome trace to this path
  #[arg(long, value_name = "PATH")]
  trace_out: Option<PathBuf>,

  /// Print full error chains on failure
  #[arg(long)]
  verbose: bool,
}

fn render_page(
  url: &str,
  output: &Path,
  bundle: RenderConfigBundle,
  render_pool: FastRenderPool,
  fetcher: Arc<dyn ResourceFetcher>,
  output_format: OutputFormat,
  base_url_override: Option<String>,
  js: JsCliConfig,
) -> Result<()> {
  let RenderConfigBundle { options, .. } = bundle;
  let mut log = |line: &str| println!("{line}");

  let (resource, requested_url) = if url.starts_with("file://") {
    let path = url.strip_prefix("file://").unwrap_or(url);
    let cached = read_cached_document(Path::new(path))?;
    (cached.resource, cached.document.base_hint)
  } else {
    println!("Fetching HTML from: {url}");
    let resource = fetcher.fetch_with_request(FetchRequest::document(url))?;
    (resource, url.to_string())
  };

  let mut resource =
    follow_client_redirects_resource(fetcher.as_ref(), resource, &requested_url, &mut log);

  let mut base_hint = resource
    .final_url
    .clone()
    .unwrap_or_else(|| requested_url.clone());
  if let Some(base_url) = base_url_override.as_deref() {
    base_hint = base_url.to_string();
    resource.final_url = Some(base_hint.clone());
  }

  let mut options = options;
  let render_result = render_pool.with_renderer(|renderer| {
    if !js.enabled {
      return render_fetched_document(renderer, &resource, Some(&base_hint), &options);
    }

    log("JavaScript: enabled");

    struct JsExecutionGuard {
      depth: std::rc::Rc<std::cell::Cell<usize>>,
    }

    impl JsExecutionGuard {
      fn enter(depth: &std::rc::Rc<std::cell::Cell<usize>>) -> Self {
        let cur = depth.get();
        depth.set(cur + 1);
        Self {
          depth: std::rc::Rc::clone(depth),
        }
      }
    }

    impl Drop for JsExecutionGuard {
      fn drop(&mut self) {
        let cur = self.depth.get();
        debug_assert!(cur > 0, "js execution depth underflow");
        self.depth.set(cur.saturating_sub(1));
      }
    }

    // Ensure JavaScript execution is bounded by the same cooperative render timeout.
    // We keep this deadline alive through the render so check_root-based operations see it too.
    let deadline = RenderDeadline::new(options.timeout, options.cancel_callback.clone());
    let _deadline_guard = DeadlineGuard::install(Some(&deadline));

    // Decode + parse the HTML with scripting-enabled semantics (affects <noscript> parsing).
    let html = fastrender::html::encoding::decode_html_bytes(
      &resource.bytes,
      resource.content_type.as_deref(),
    );
    let mut event_loop = EventLoop::<WindowHostState>::new();
    let clock = event_loop.clock();
    let dom2 = Dom2Document::new(QuirksMode::NoQuirks);
    let mut host = match WindowHostState::new_with_fetcher_and_clock(dom2, base_hint.clone(), fetcher.clone(), clock) {
      Ok(host) => host,
      Err(err) => {
        log(&format!(
          "JavaScript: failed to initialize window host (continuing without JS): {err}"
        ));
        return render_fetched_document(renderer, &resource, Some(&base_hint), &options);
      }
    };

    let run_limits = js.run_limits();
    let max_script_bytes = if js.max_script_bytes == 0 {
      usize::MAX
    } else {
      js.max_script_bytes
    };
    let js_execution_depth: std::rc::Rc<std::cell::Cell<usize>> =
      std::rc::Rc::new(std::cell::Cell::new(0));
    let orchestrator = std::rc::Rc::new(std::cell::RefCell::new(ScriptOrchestrator::new()));
    let scheduler = std::rc::Rc::new(std::cell::RefCell::new(
      ScriptScheduler::<fastrender::dom2::NodeId>::new(),
    ));

    let mut scripts_queued = 0usize;
    let module_loader = Rc::new(RefCell::new(ModuleGraphLoader::new(fetcher.clone())));

    let mut run_state = event_loop.new_run_state(run_limits);
    let mut js_active = true;

    fn run_event_loop_until_idle_limited_handling_errors(
      event_loop: &mut EventLoop<WindowHostState>,
      host: &mut WindowHostState,
      run_state: &mut RunState,
      log: &mut dyn FnMut(&str),
    ) -> Result<RunUntilIdleOutcome> {
      loop {
        // Drain microtasks first (HTML microtask checkpoint).
        let microtasks_before = run_state.microtasks_executed();
        match event_loop.perform_microtask_checkpoint_limited(host, run_state) {
          Ok(MicrotaskCheckpointLimitedOutcome::Completed) => {}
          Ok(MicrotaskCheckpointLimitedOutcome::Stopped(reason)) => {
            return Ok(RunUntilIdleOutcome::Stopped(reason));
          }
          Err(err) => {
            let progressed = run_state.microtasks_executed() != microtasks_before;
            if progressed {
              log(&format!("JavaScript: uncaught exception: {err}"));
              continue;
            }
            return Err(err);
          }
        }

        let tasks_before = run_state.tasks_executed();
        let microtasks_before = run_state.microtasks_executed();
        match event_loop.run_next_task_limited(host, run_state) {
          Ok(RunNextTaskLimitedOutcome::Ran) => continue,
          Ok(RunNextTaskLimitedOutcome::NoTask) => {
            if event_loop.is_idle() {
              return Ok(RunUntilIdleOutcome::Idle);
            }
            continue;
          }
          Ok(RunNextTaskLimitedOutcome::Stopped(reason)) => {
            return Ok(RunUntilIdleOutcome::Stopped(reason));
          }
          Err(err) => {
            let progressed = run_state.tasks_executed() != tasks_before
              || run_state.microtasks_executed() != microtasks_before;
            if progressed {
              log(&format!("JavaScript: uncaught exception: {err}"));
              continue;
            }
            return Err(err);
          }
        }
      }
    }

    let mut parser = StreamingHtmlParser::new(Some(&base_hint));

    // Feed decoded HTML incrementally (mirrors BrowserTab's navigation path). This keeps parsing
    // cancellable and gives async script tasks an opportunity to run between chunks.
    const INPUT_CHUNK_BYTES: usize = 8 * 1024;
    let mut offset = 0usize;
    while offset < html.len() {
      let mut end = (offset + INPUT_CHUNK_BYTES).min(html.len());
      while end < html.len() && !html.is_char_boundary(end) {
        end += 1;
      }
      debug_assert!(html.is_char_boundary(offset));
      debug_assert!(html.is_char_boundary(end));
      parser.push_str(&html[offset..end]);
      offset = end;

      loop {
        match parser.pump() {
          Ok(StreamingParserYield::Script {
            script,
            base_url_at_this_point,
          }) => {
            if !js_active {
              // Skip script execution and keep parsing.
              continue;
            }

            let snapshot = {
              let Some(doc) = parser.document() else {
                log("JavaScript: streaming parser yielded a script without an active document (continuing)");
                js_active = false;
                event_loop.clear_all_pending_work();
                break;
              };
              doc.clone_with_events()
            };

            let base_url = base_url_at_this_point.clone();
            host.base_url = base_url.clone();
            host.window_mut().set_base_url(base_url);
            host.mutate_dom(|dom| {
              *dom = snapshot;
              ((), true)
            });

            let mut executed_sync_script = false;
            let mut stop_reason: Option<RunUntilIdleStopReason> = None;
            let mut fatal_error: Option<fastrender::Error> = None;

            // Install the streaming parser so `document.write()` can inject while we execute scripts
            // and/or run event loop turns during parsing.
            parser.with_active_document_write(|| {
              // HTML: before preparing a parser-inserted script at a script end-tag boundary, perform
              // a microtask checkpoint when the JS execution context stack is empty.
              if js_execution_depth.get() == 0 {
                let before = run_state.microtasks_executed();
                match event_loop.perform_microtask_checkpoint_limited(&mut host, &mut run_state) {
                  Ok(MicrotaskCheckpointLimitedOutcome::Completed) => {}
                  Ok(MicrotaskCheckpointLimitedOutcome::Stopped(reason)) => {
                    stop_reason = Some(reason);
                    return;
                  }
                  Err(err) => {
                    let progressed = run_state.microtasks_executed() != before;
                    if progressed {
                      log(&format!("JavaScript: uncaught exception in microtask checkpoint: {err}"));
                    } else {
                      fatal_error = Some(err);
                      return;
                    }
                  }
                }
              }

              if stop_reason.is_some() || fatal_error.is_some() {
                return;
              }

              let base = BaseUrlTracker::new(base_url_at_this_point.as_deref());
              let spec = build_parser_inserted_script_element_spec_dom2(host.dom(), script, &base);

              // Module scripts are not modeled by `ScriptScheduler` yet; execute them via the minimal
              // `ModuleGraphLoader` bundler so `type="module"` fixtures work in `--js` mode.
              if spec.script_type == ScriptType::Module {
                if spec.src_attr_present {
                  if let Some(resolved_src) = spec.src.clone().filter(|s| !s.is_empty()) {
                    let loader = Rc::clone(&module_loader);
                    let entry_url: Arc<str> = Arc::from(resolved_src.clone());
                    let script_name: Arc<str> = Arc::from(format!("<module script {resolved_src}>"));
                    if let Err(err) =
                      event_loop.queue_task(TaskSource::Script, move |host, event_loop| {
                        let prev = host.current_script_state().borrow().current_script;
                        host.current_script_state().borrow_mut().current_script = None;

                        let result = (|| {
                          let bundle =
                            loader.borrow_mut().build_bundle_for_url(&entry_url, max_script_bytes)?;
                          let bundle_text: Arc<str> = Arc::from(bundle);

                          {
                            let window = host.window_mut();
                            window.reset_interrupt();
                            window.vm_mut().set_budget(js_budget_for_script(run_limits));
                          }
                          let result = host.exec_script_with_name_in_event_loop(
                            event_loop,
                            script_name.clone(),
                            bundle_text,
                          );
                          {
                            let window = host.window_mut();
                            window
                              .vm_mut()
                              .set_budget(Budget::unlimited(DEFAULT_JS_CHECK_TIME_EVERY));
                          }
                          result.map(|_| ())
                        })();

                        host.current_script_state().borrow_mut().current_script = prev;
                        result.map_err(|err| {
                          fastrender::Error::Other(format!("{}: {err}", &*script_name))
                        })
                      })
                    {
                      log(&format!("JavaScript: failed to queue module script task: {err}"));
                    } else {
                      scripts_queued += 1;
                    }
                  } else {
                    log("JavaScript: skipping <script type=module src> with empty/invalid/unresolvable src");
                  }
                } else if spec.inline_text.as_bytes().len() > max_script_bytes {
                  log(&format!(
                    "JavaScript: skipping inline module script ({} bytes > max {})",
                    spec.inline_text.as_bytes().len(),
                    max_script_bytes
                  ));
                } else {
                  let loader = Rc::clone(&module_loader);
                  let inline_id: Arc<str> =
                    Arc::from(format!("{base_hint}#inline-module-{}", script.index()));
                  let inline_base_url: Arc<str> =
                    Arc::from(spec.base_url.clone().unwrap_or_else(|| base_hint.clone()));
                  let script_name: Arc<str> =
                    Arc::from(format!("<inline module script {}>", script.index()));
                  let script_text: Arc<str> = Arc::from(spec.inline_text.clone());
                  if let Err(err) =
                    event_loop.queue_task(TaskSource::Script, move |host, event_loop| {
                      let prev = host.current_script_state().borrow().current_script;
                      host.current_script_state().borrow_mut().current_script = None;

                      let result = (|| {
                        let bundle = loader.borrow_mut().build_bundle_for_inline(
                          &inline_id,
                          &inline_base_url,
                          &script_text,
                          max_script_bytes,
                        )?;
                        let bundle_text: Arc<str> = Arc::from(bundle);

                        {
                          let window = host.window_mut();
                          window.reset_interrupt();
                          window.vm_mut().set_budget(js_budget_for_script(run_limits));
                        }
                        let result = host.exec_script_with_name_in_event_loop(
                          event_loop,
                          script_name.clone(),
                          bundle_text,
                        );
                        {
                          let window = host.window_mut();
                          window
                            .vm_mut()
                            .set_budget(Budget::unlimited(DEFAULT_JS_CHECK_TIME_EVERY));
                        }
                        result.map(|_| ())
                      })();

                      host.current_script_state().borrow_mut().current_script = prev;
                      result.map_err(|err| {
                        fastrender::Error::Other(format!("{}: {err}", &*script_name))
                      })
                    })
                  {
                    log(&format!("JavaScript: failed to queue inline module script task: {err}"));
                  } else {
                    scripts_queued += 1;
                  }
                }
              }
              let base_url_at_discovery = spec.base_url.clone();

              let discovered = match scheduler
                .borrow_mut()
                .discovered_parser_script(spec, script, base_url_at_discovery)
              {
                Ok(discovered) => discovered,
                Err(err) => {
                  log(&format!("JavaScript: scheduler error at script boundary (continuing): {err}"));
                  // Keep parsing.
                  return;
                }
              };

              // Apply scheduler actions produced by discovery.
              let mut actions = discovered.actions;
              'apply_actions: loop {
                let mut start_fetches: Vec<(
                  ScriptId,
                  fastrender::dom2::NodeId,
                  String,
                  FetchDestination,
                )> = Vec::new();
                let mut blocking: std::collections::HashSet<ScriptId> =
                  std::collections::HashSet::new();

                for action in actions.drain(..) {
                  match action {
                    ScriptSchedulerAction::StartFetch {
                      script_id,
                      node_id,
                      url,
                      destination,
                    } => {
                      start_fetches.push((script_id, node_id, url, destination));
                    }
                    ScriptSchedulerAction::BlockParserUntilExecuted { script_id, .. } => {
                      blocking.insert(script_id);
                    }
                    ScriptSchedulerAction::ExecuteNow {
                      script_id: _,
                      node_id,
                      source_text,
                      ..
                    } => {
                      executed_sync_script = true;
                      if source_text.as_bytes().len() > max_script_bytes {
                        log(&format!(
                          "JavaScript: skipping script ({} bytes > max {})",
                          source_text.as_bytes().len(),
                          max_script_bytes
                        ));
                        continue;
                      }

                      struct Adapter<'a> {
                        name: Arc<str>,
                        source_text: &'a str,
                        event_loop: &'a mut EventLoop<WindowHostState>,
                        run_limits: RunLimits,
                      }

                      impl fastrender::js::ScriptBlockExecutor<WindowHostState> for Adapter<'_> {
                        fn execute_script(
                          &mut self,
                          host: &mut WindowHostState,
                          _orchestrator: &mut ScriptOrchestrator,
                          _script: fastrender::dom2::NodeId,
                          script_type: ScriptType,
                        ) -> Result<()> {
                          if script_type != ScriptType::Classic {
                            return Ok(());
                          }

                          {
                            let window = host.window_mut();
                            window.reset_interrupt();
                            window.vm_mut().set_budget(js_budget_for_script(self.run_limits));
                          }
                          let result = host.exec_script_with_name_in_event_loop(
                            self.event_loop,
                            self.name.clone(),
                            Arc::from(self.source_text),
                          );
                          {
                            let window = host.window_mut();
                            window
                              .vm_mut()
                              .set_budget(Budget::unlimited(DEFAULT_JS_CHECK_TIME_EVERY));
                          }

                          result.map(|_| ())
                        }
                      }

                      let _guard = JsExecutionGuard::enter(&js_execution_depth);
                      let name: Arc<str> = Arc::from(format!("<script {}>", node_id.index()));
                      let mut adapter = Adapter {
                        name: name.clone(),
                        source_text: &source_text,
                        event_loop: &mut event_loop,
                        run_limits,
                      };
                      let exec_result = orchestrator
                        .borrow_mut()
                        .execute_script_element(
                          &mut host,
                          node_id,
                          ScriptType::Classic,
                          &mut adapter,
                        );
                      if let Err(err) = exec_result {
                        log(&format!("JavaScript: uncaught exception: {err}"));
                      }

                      // HTML: "clean up after running script" performs a microtask checkpoint only when
                      // the JS execution context stack is empty.
                      if js_execution_depth.get() == 0 {
                        let before = run_state.microtasks_executed();
                        match event_loop.perform_microtask_checkpoint_limited(&mut host, &mut run_state)
                        {
                          Ok(MicrotaskCheckpointLimitedOutcome::Completed) => {}
                          Ok(MicrotaskCheckpointLimitedOutcome::Stopped(reason)) => {
                            stop_reason = Some(reason);
                            return;
                          }
                          Err(err) => {
                            let progressed = run_state.microtasks_executed() != before;
                            if progressed {
                              log(&format!(
                                "JavaScript: uncaught exception in microtask checkpoint: {err}"
                              ));
                            } else {
                              fatal_error = Some(err);
                              return;
                            }
                          }
                        }
                      }
                    }
                    ScriptSchedulerAction::QueueTask {
                      node_id,
                      source_text,
                      ..
                    } => {
                      if source_text.as_bytes().len() > max_script_bytes {
                        log(&format!(
                          "JavaScript: skipping script task ({} bytes > max {})",
                          source_text.as_bytes().len(),
                          max_script_bytes
                        ));
                        continue;
                      }

                      let orchestrator = std::rc::Rc::clone(&orchestrator);
                      let js_execution_depth = std::rc::Rc::clone(&js_execution_depth);
                      let source_text: Arc<str> = Arc::from(source_text);
                      let name: Arc<str> = Arc::from(format!("<script {}>", node_id.index()));
                      if let Err(err) =
                        event_loop.queue_task(TaskSource::Script, move |host, event_loop| {
                          let _guard = JsExecutionGuard::enter(&js_execution_depth);

                          struct Adapter<'a> {
                            name: Arc<str>,
                            source_text: Arc<str>,
                            event_loop: &'a mut EventLoop<WindowHostState>,
                            run_limits: RunLimits,
                          }
                          impl fastrender::js::ScriptBlockExecutor<WindowHostState> for Adapter<'_> {
                            fn execute_script(
                              &mut self,
                              host: &mut WindowHostState,
                              _orchestrator: &mut ScriptOrchestrator,
                              _script: fastrender::dom2::NodeId,
                              script_type: ScriptType,
                            ) -> Result<()> {
                              if script_type != ScriptType::Classic {
                                return Ok(());
                              }
                              {
                                let window = host.window_mut();
                                window.reset_interrupt();
                                window.vm_mut().set_budget(js_budget_for_script(self.run_limits));
                              }
                              let result = host.exec_script_with_name_in_event_loop(
                                self.event_loop,
                                self.name.clone(),
                                self.source_text.clone(),
                              );
                              {
                                let window = host.window_mut();
                                window
                                  .vm_mut()
                                  .set_budget(Budget::unlimited(DEFAULT_JS_CHECK_TIME_EVERY));
                              }
                              result.map(|_| ())
                            }
                          }

                          let mut adapter = Adapter {
                            name: name.clone(),
                            source_text: source_text.clone(),
                            event_loop,
                            run_limits,
                          };
                          let exec_result = orchestrator
                            .borrow_mut()
                            .execute_script_element(host, node_id, ScriptType::Classic, &mut adapter);
                          exec_result
                            .map_err(|err| fastrender::Error::Other(format!("{}: {err}", &*name)))
                        })
                      {
                        log(&format!("JavaScript: failed to queue script task: {err}"));
                      } else {
                        scripts_queued += 1;
                      }
                    }
                    ScriptSchedulerAction::QueueScriptEventTask { .. } => {}
                  }
                }

                for (script_id, node_id, url, destination) in start_fetches.drain(..) {
                  if blocking.contains(&script_id) {
                    let charset_attr =
                      host.dom().get_attribute(node_id, "charset").ok().flatten();
                    let fetched = match fetch_script_source(
                      fetcher.as_ref(),
                      &base_hint,
                      &url,
                      destination,
                      max_script_bytes,
                    ) {
                      Ok(fetched) => fetched,
                      Err(err) => {
                        log(&format!(
                          "JavaScript: failed to fetch blocking script {url} (continuing): {err}"
                        ));
                        actions = match scheduler.borrow_mut().fetch_failed(script_id) {
                          Ok(actions) => actions,
                          Err(err) => {
                            log(&format!(
                              "JavaScript: scheduler error after failed fetch (continuing): {err}"
                            ));
                            Vec::new()
                          }
                        };
                        continue 'apply_actions;
                      }
                    };
                    if max_script_bytes != usize::MAX && fetched.bytes.len() > max_script_bytes {
                      log(&format!(
                        "JavaScript: skipping external script {url} ({} bytes > max {})",
                        fetched.bytes.len(),
                        max_script_bytes
                      ));
                      actions = match scheduler.borrow_mut().fetch_failed(script_id) {
                        Ok(actions) => actions,
                        Err(err) => {
                          log(&format!(
                            "JavaScript: scheduler error after failed fetch (continuing): {err}"
                          ));
                          Vec::new()
                        }
                      };
                      continue 'apply_actions;
                    }
                    let source_text = decode_external_classic_script_bytes(
                      &fetched.bytes,
                      fetched.content_type.as_deref(),
                      charset_attr,
                    );
                    actions = match scheduler.borrow_mut().fetch_completed(script_id, source_text) {
                      Ok(actions) => actions,
                      Err(err) => {
                        log(&format!("JavaScript: scheduler error after fetch (continuing): {err}"));
                        Vec::new()
                      }
                    };
                    // Apply actions from fetch completion (may execute now) before resuming parsing.
                    continue 'apply_actions;
                  }

                  let scheduler = std::rc::Rc::clone(&scheduler);
                  let orchestrator = std::rc::Rc::clone(&orchestrator);
                  let js_execution_depth = std::rc::Rc::clone(&js_execution_depth);
                  let node_id_for_task = node_id;
                  let url_for_task = url.clone();
                  let destination_for_task = destination;
                  let fetcher = fetcher.clone();
                  if let Err(err) = event_loop.queue_task(
                    TaskSource::Networking,
                    move |host, event_loop| {
                      let charset_attr = host
                        .dom()
                        .get_attribute(node_id_for_task, "charset")
                        .ok()
                        .flatten();
                      let fetched = fetch_script_source(
                        fetcher.as_ref(),
                        &host.document_url,
                        &url_for_task,
                        destination_for_task,
                        max_script_bytes,
                      );
                      let actions = match fetched {
                        Ok(fetched) => {
                          if max_script_bytes != usize::MAX && fetched.bytes.len() > max_script_bytes {
                            scheduler.borrow_mut().fetch_failed(script_id)?
                          } else {
                            let source_text = decode_external_classic_script_bytes(
                              &fetched.bytes,
                              fetched.content_type.as_deref(),
                              charset_attr,
                            );
                            scheduler.borrow_mut().fetch_completed(script_id, source_text)?
                          }
                        }
                        Err(_err) => scheduler.borrow_mut().fetch_failed(script_id)?,
                      };

                      // Apply resulting actions for this *non-blocking* external script (async/defer).
                      let mut queued: Vec<(ScriptId, fastrender::dom2::NodeId, String)> = Vec::new();
                      for action in actions {
                        match action {
                          ScriptSchedulerAction::QueueTask { node_id, source_text, .. } => {
                            queued.push((script_id, node_id, source_text));
                          }
                          ScriptSchedulerAction::ExecuteNow { node_id, source_text, .. } => {
                            // ExecuteNow is unexpected here, but run it anyway.
                            let _guard = JsExecutionGuard::enter(&js_execution_depth);
                            struct Adapter<'a> {
                              name: Arc<str>,
                              source_text: Arc<str>,
                              event_loop: &'a mut EventLoop<WindowHostState>,
                              run_limits: RunLimits,
                            }
                            impl fastrender::js::ScriptBlockExecutor<WindowHostState> for Adapter<'_> {
                              fn execute_script(
                                &mut self,
                                host: &mut WindowHostState,
                                _orchestrator: &mut ScriptOrchestrator,
                                _script: fastrender::dom2::NodeId,
                                script_type: ScriptType,
                              ) -> Result<()> {
                                if script_type != ScriptType::Classic {
                                  return Ok(());
                                }
                                {
                                  let window = host.window_mut();
                                  window.reset_interrupt();
                                  window.vm_mut().set_budget(js_budget_for_script(self.run_limits));
                                }
                                let result = host.exec_script_with_name_in_event_loop(
                                  self.event_loop,
                                  self.name.clone(),
                                  self.source_text.clone(),
                                );
                                {
                                  let window = host.window_mut();
                                  window
                                    .vm_mut()
                                    .set_budget(Budget::unlimited(DEFAULT_JS_CHECK_TIME_EVERY));
                                }
                                result.map(|_| ())
                              }
                            }
                            let name: Arc<str> = Arc::from(format!("<script {}>", node_id.index()));
                            let mut adapter = Adapter {
                              name: name.clone(),
                              source_text: Arc::from(source_text),
                              event_loop,
                              run_limits,
                            };
                            let _ = orchestrator
                              .borrow_mut()
                              .execute_script_element(host, node_id, ScriptType::Classic, &mut adapter);
                          }
                          _ => {}
                        }
                      }

                      for (_id, node_id, source_text) in queued {
                        let orchestrator = std::rc::Rc::clone(&orchestrator);
                        let js_execution_depth = std::rc::Rc::clone(&js_execution_depth);
                        let source_text: Arc<str> = Arc::from(source_text);
                        let name: Arc<str> = Arc::from(format!("<script {}>", node_id.index()));
                        event_loop.queue_task(TaskSource::Script, move |host, event_loop| {
                          let _guard = JsExecutionGuard::enter(&js_execution_depth);
                          struct Adapter<'a> {
                            name: Arc<str>,
                            source_text: Arc<str>,
                            event_loop: &'a mut EventLoop<WindowHostState>,
                            run_limits: RunLimits,
                          }
                          impl fastrender::js::ScriptBlockExecutor<WindowHostState> for Adapter<'_> {
                            fn execute_script(
                              &mut self,
                              host: &mut WindowHostState,
                              _orchestrator: &mut ScriptOrchestrator,
                              _script: fastrender::dom2::NodeId,
                              script_type: ScriptType,
                            ) -> Result<()> {
                              if script_type != ScriptType::Classic {
                                return Ok(());
                              }
                              {
                                let window = host.window_mut();
                                window.reset_interrupt();
                                window.vm_mut().set_budget(js_budget_for_script(self.run_limits));
                              }
                              let result = host.exec_script_with_name_in_event_loop(
                                self.event_loop,
                                self.name.clone(),
                                self.source_text.clone(),
                              );
                              {
                                let window = host.window_mut();
                                window
                                  .vm_mut()
                                  .set_budget(Budget::unlimited(DEFAULT_JS_CHECK_TIME_EVERY));
                              }
                              result.map(|_| ())
                            }
                          }
                          let mut adapter = Adapter {
                            name: name.clone(),
                            source_text: source_text.clone(),
                            event_loop,
                            run_limits,
                          };
                          orchestrator
                            .borrow_mut()
                            .execute_script_element(host, node_id, ScriptType::Classic, &mut adapter)
                            .map_err(|err| fastrender::Error::Other(format!("{}: {err}", &*name)))
                        })?;
                      }

                      Ok(())
                    },
                  ) {
                    log(&format!("JavaScript: failed to queue script fetch task: {err}"));
                  } else {
                    scripts_queued += 1;
                  }
                }

                break;
              }
            });

            if let Some(err) = fatal_error {
              js_active = false;
              event_loop.clear_all_pending_work();
              log(&format!("JavaScript: error while running JS (continuing): {err}"));
            } else if let Some(reason) = stop_reason {
              js_active = false;
              event_loop.clear_all_pending_work();
              match reason {
                RunUntilIdleStopReason::MaxTasks { executed, limit } => {
                  log(&format!(
                    "JavaScript: stopped after max tasks (executed {executed} / limit {limit})"
                  ));
                }
                RunUntilIdleStopReason::MaxMicrotasks { executed, limit } => {
                  log(&format!(
                    "JavaScript: stopped after max microtasks (executed {executed} / limit {limit})"
                  ));
                }
                RunUntilIdleStopReason::WallTime { elapsed, limit } => {
                  log(&format!(
                    "JavaScript: stopped after wall time (elapsed {}ms / limit {}ms)",
                    elapsed.as_millis(),
                    limit.as_millis()
                  ));
                }
              }
            }

            // Sync any DOM mutations from executed scripts back into the streaming parser's live DOM
            // before resuming parsing.
            let updated = host.dom().clone_with_events();
            if let Some(mut doc) = parser.document_mut() {
              *doc = updated;
            }

            // If this script boundary did not execute a synchronous script (i.e. async/defer/module),
            // allow queued async script tasks to run before the parser advances further. This mirrors
            // the HTML requirement that async scripts execute "when ready", independent of parser
            // progress, while still ensuring `document.write()` output from parser-blocking scripts is
            // parsed before yielding to tasks.
            if js_active && !executed_sync_script && !event_loop.is_idle() {
              match parser.with_active_document_write(|| {
                run_event_loop_until_idle_limited_handling_errors(
                  &mut event_loop,
                  &mut host,
                  &mut run_state,
                  &mut log,
                )
              }) {
                Ok(RunUntilIdleOutcome::Idle) => {}
                Ok(RunUntilIdleOutcome::Stopped(reason)) => {
                  js_active = false;
                  event_loop.clear_all_pending_work();
                  match reason {
                    RunUntilIdleStopReason::MaxTasks { executed, limit } => {
                      log(&format!(
                        "JavaScript: stopped after max tasks (executed {executed} / limit {limit})"
                      ));
                    }
                    RunUntilIdleStopReason::MaxMicrotasks { executed, limit } => {
                      log(&format!(
                        "JavaScript: stopped after max microtasks (executed {executed} / limit {limit})"
                      ));
                    }
                    RunUntilIdleStopReason::WallTime { elapsed, limit } => {
                      log(&format!(
                        "JavaScript: stopped after wall time (elapsed {}ms / limit {}ms)",
                        elapsed.as_millis(),
                        limit.as_millis()
                      ));
                    }
                  }
                }
                Err(err) => {
                  js_active = false;
                  event_loop.clear_all_pending_work();
                  log(&format!("JavaScript: error while running event loop (continuing): {err}"));
                }
              }

              let updated = host.dom().clone_with_events();
              if let Some(mut doc) = parser.document_mut() {
                *doc = updated;
              }
            }
          }
          Ok(StreamingParserYield::NeedMoreInput) => break,
          Ok(StreamingParserYield::Finished { .. }) => {
            log("JavaScript: streaming HTML parser unexpectedly finished before EOF (continuing without JS)");
            js_active = false;
            event_loop.clear_all_pending_work();
            break;
          }
          Err(err) => {
            log(&format!("JavaScript: failed to parse HTML (continuing without JS): {err}"));
            return render_fetched_document(renderer, &resource, Some(&base_hint), &options);
          }
        }
      }

      // Between input chunks, allow queued tasks (async script fetch/execution, timers, etc.) to
      // run while parsing is still in progress.
      if js_active && !event_loop.is_idle() {
        let snapshot = {
          let Some(doc) = parser.document() else {
            js_active = false;
            event_loop.clear_all_pending_work();
            continue;
          };
          doc.clone_with_events()
        };
        let base_url = parser.current_base_url();
        host.base_url = base_url.clone();
        host.window_mut().set_base_url(base_url);
        host.mutate_dom(|dom| {
          *dom = snapshot;
          ((), true)
        });

        match parser.with_active_document_write(|| {
          run_event_loop_until_idle_limited_handling_errors(
            &mut event_loop,
            &mut host,
            &mut run_state,
            &mut log,
          )
        }) {
          Ok(RunUntilIdleOutcome::Idle) => {}
          Ok(RunUntilIdleOutcome::Stopped(reason)) => {
            js_active = false;
            event_loop.clear_all_pending_work();
            match reason {
              RunUntilIdleStopReason::MaxTasks { executed, limit } => {
                log(&format!(
                  "JavaScript: stopped after max tasks (executed {executed} / limit {limit})"
                ));
              }
              RunUntilIdleStopReason::MaxMicrotasks { executed, limit } => {
                log(&format!(
                  "JavaScript: stopped after max microtasks (executed {executed} / limit {limit})"
                ));
              }
              RunUntilIdleStopReason::WallTime { elapsed, limit } => {
                log(&format!(
                  "JavaScript: stopped after wall time (elapsed {}ms / limit {}ms)",
                  elapsed.as_millis(),
                  limit.as_millis()
                ));
              }
            }
          }
          Err(err) => {
            js_active = false;
            event_loop.clear_all_pending_work();
            log(&format!("JavaScript: error while running event loop (continuing): {err}"));
          }
        }

        let updated = host.dom().clone_with_events();
        if let Some(mut doc) = parser.document_mut() {
          *doc = updated;
        }
      }
    }

    parser.set_eof();

    loop {
      match parser.pump() {
        Ok(StreamingParserYield::Script { .. }) if !js_active => {
          // JS execution is disabled (limits/error); keep parsing.
          continue;
        }
        Ok(StreamingParserYield::Script {
          script,
          base_url_at_this_point,
        }) => {
          let snapshot = {
            let Some(doc) = parser.document() else {
              log("JavaScript: streaming parser yielded a script without an active document (continuing without JS)");
              js_active = false;
              event_loop.clear_all_pending_work();
              continue;
            };
            doc.clone_with_events()
          };

          let base_url = base_url_at_this_point.clone();
          host.base_url = base_url.clone();
          host.window_mut().set_base_url(base_url);
          host.mutate_dom(|dom| {
            *dom = snapshot;
            ((), true)
          });

          let mut executed_sync_script = false;
          let mut stop_reason: Option<RunUntilIdleStopReason> = None;
          let mut fatal_error: Option<fastrender::Error> = None;

          parser.with_active_document_write(|| {
            // HTML: before preparing a parser-inserted script at a script end-tag boundary, perform
            // a microtask checkpoint when the JS execution context stack is empty.
            if js_execution_depth.get() == 0 {
              let before = run_state.microtasks_executed();
              match event_loop.perform_microtask_checkpoint_limited(&mut host, &mut run_state) {
                Ok(MicrotaskCheckpointLimitedOutcome::Completed) => {}
                Ok(MicrotaskCheckpointLimitedOutcome::Stopped(reason)) => {
                  stop_reason = Some(reason);
                  return;
                }
                Err(err) => {
                  let progressed = run_state.microtasks_executed() != before;
                  if progressed {
                    log(&format!("JavaScript: uncaught exception in microtask checkpoint: {err}"));
                  } else {
                    fatal_error = Some(err);
                    return;
                  }
                }
              }
            }

            if stop_reason.is_some() || fatal_error.is_some() {
              return;
            }

            let base = BaseUrlTracker::new(base_url_at_this_point.as_deref());
            let spec = build_parser_inserted_script_element_spec_dom2(host.dom(), script, &base);

            // Module scripts are not modeled by `ScriptScheduler` yet; execute them via the minimal
            // `ModuleGraphLoader` bundler so `type="module"` fixtures work in `--js` mode.
            if spec.script_type == ScriptType::Module {
              if spec.src_attr_present {
                if let Some(resolved_src) = spec.src.clone().filter(|s| !s.is_empty()) {
                  let loader = Rc::clone(&module_loader);
                  let entry_url: Arc<str> = Arc::from(resolved_src.clone());
                  let script_name: Arc<str> = Arc::from(format!("<module script {resolved_src}>"));
                  if let Err(err) = event_loop.queue_task(TaskSource::Script, move |host, event_loop| {
                    let prev = host.current_script_state().borrow().current_script;
                    host.current_script_state().borrow_mut().current_script = None;

                    let result = (|| {
                      let bundle = loader.borrow_mut().build_bundle_for_url(&entry_url, max_script_bytes)?;
                      let bundle_text: Arc<str> = Arc::from(bundle);

                      {
                        let window = host.window_mut();
                        window.reset_interrupt();
                        window.vm_mut().set_budget(js_budget_for_script(run_limits));
                      }
                      let result = host.exec_script_with_name_in_event_loop(
                        event_loop,
                        script_name.clone(),
                        bundle_text,
                      );
                      {
                        let window = host.window_mut();
                        window
                          .vm_mut()
                          .set_budget(Budget::unlimited(DEFAULT_JS_CHECK_TIME_EVERY));
                      }
                      result.map(|_| ())
                    })();

                    host.current_script_state().borrow_mut().current_script = prev;
                    result.map_err(|err| fastrender::Error::Other(format!("{}: {err}", &*script_name)))
                  }) {
                    log(&format!("JavaScript: failed to queue module script task: {err}"));
                  } else {
                    scripts_queued += 1;
                  }
                } else {
                  log("JavaScript: skipping <script type=module src> with empty/invalid/unresolvable src");
                }
              } else if spec.inline_text.as_bytes().len() > max_script_bytes {
                log(&format!(
                  "JavaScript: skipping inline module script ({} bytes > max {})",
                  spec.inline_text.as_bytes().len(),
                  max_script_bytes
                ));
              } else {
                let loader = Rc::clone(&module_loader);
                let inline_id: Arc<str> = Arc::from(format!("{base_hint}#inline-module-{}", script.index()));
                let inline_base_url: Arc<str> =
                  Arc::from(spec.base_url.clone().unwrap_or_else(|| base_hint.clone()));
                let script_name: Arc<str> = Arc::from(format!("<inline module script {}>", script.index()));
                let script_text: Arc<str> = Arc::from(spec.inline_text.clone());
                if let Err(err) = event_loop.queue_task(TaskSource::Script, move |host, event_loop| {
                  let prev = host.current_script_state().borrow().current_script;
                  host.current_script_state().borrow_mut().current_script = None;

                  let result = (|| {
                    let bundle = loader.borrow_mut().build_bundle_for_inline(
                      &inline_id,
                      &inline_base_url,
                      &script_text,
                      max_script_bytes,
                    )?;
                    let bundle_text: Arc<str> = Arc::from(bundle);

                    {
                      let window = host.window_mut();
                      window.reset_interrupt();
                      window.vm_mut().set_budget(js_budget_for_script(run_limits));
                    }
                    let result = host.exec_script_with_name_in_event_loop(
                      event_loop,
                      script_name.clone(),
                      bundle_text,
                    );
                    {
                      let window = host.window_mut();
                      window
                        .vm_mut()
                        .set_budget(Budget::unlimited(DEFAULT_JS_CHECK_TIME_EVERY));
                    }
                    result.map(|_| ())
                  })();

                  host.current_script_state().borrow_mut().current_script = prev;
                  result.map_err(|err| fastrender::Error::Other(format!("{}: {err}", &*script_name)))
                }) {
                  log(&format!("JavaScript: failed to queue inline module script task: {err}"));
                } else {
                  scripts_queued += 1;
                }
              }
            }

            let base_url_at_discovery = spec.base_url.clone();
            let discovered = match scheduler
              .borrow_mut()
              .discovered_parser_script(spec, script, base_url_at_discovery)
            {
              Ok(discovered) => discovered,
              Err(err) => {
                log(&format!("JavaScript: scheduler error at script boundary (continuing): {err}"));
                return;
              }
            };

            let mut actions = discovered.actions;
            'apply_actions: loop {
              let mut start_fetches: Vec<(
                ScriptId,
                fastrender::dom2::NodeId,
                String,
                FetchDestination,
              )> = Vec::new();
              let mut blocking: std::collections::HashSet<ScriptId> = std::collections::HashSet::new();

              for action in actions.drain(..) {
                match action {
                  ScriptSchedulerAction::StartFetch {
                    script_id,
                    node_id,
                    url,
                    destination,
                  } => {
                    start_fetches.push((script_id, node_id, url, destination));
                  }
                  ScriptSchedulerAction::BlockParserUntilExecuted { script_id, .. } => {
                    blocking.insert(script_id);
                  }
                  ScriptSchedulerAction::ExecuteNow { node_id, source_text, .. } => {
                    executed_sync_script = true;
                    if source_text.as_bytes().len() > max_script_bytes {
                      log(&format!(
                        "JavaScript: skipping script ({} bytes > max {})",
                        source_text.as_bytes().len(),
                        max_script_bytes
                      ));
                      continue;
                    }

                    struct Adapter<'a> {
                      name: Arc<str>,
                      source_text: &'a str,
                      event_loop: &'a mut EventLoop<WindowHostState>,
                      run_limits: RunLimits,
                    }
                    impl fastrender::js::ScriptBlockExecutor<WindowHostState> for Adapter<'_> {
                      fn execute_script(
                        &mut self,
                        host: &mut WindowHostState,
                        _orchestrator: &mut ScriptOrchestrator,
                        _script: fastrender::dom2::NodeId,
                        script_type: ScriptType,
                      ) -> Result<()> {
                        if script_type != ScriptType::Classic {
                          return Ok(());
                        }

                        {
                          let window = host.window_mut();
                          window.reset_interrupt();
                          window.vm_mut().set_budget(js_budget_for_script(self.run_limits));
                        }
                        let result = host.exec_script_with_name_in_event_loop(
                          self.event_loop,
                          self.name.clone(),
                          Arc::from(self.source_text),
                        );
                        {
                          let window = host.window_mut();
                          window
                            .vm_mut()
                            .set_budget(Budget::unlimited(DEFAULT_JS_CHECK_TIME_EVERY));
                        }

                        result.map(|_| ())
                      }
                    }

                    let _guard = JsExecutionGuard::enter(&js_execution_depth);
                    let name: Arc<str> = Arc::from(format!("<script {}>", node_id.index()));
                    let mut adapter = Adapter {
                      name: name.clone(),
                      source_text: &source_text,
                      event_loop: &mut event_loop,
                      run_limits,
                    };
                    let exec_result = orchestrator
                      .borrow_mut()
                      .execute_script_element(&mut host, node_id, ScriptType::Classic, &mut adapter);
                    if let Err(err) = exec_result {
                      log(&format!("JavaScript: uncaught exception: {err}"));
                    }

                    if js_execution_depth.get() == 0 {
                      let before = run_state.microtasks_executed();
                      match event_loop.perform_microtask_checkpoint_limited(&mut host, &mut run_state) {
                        Ok(MicrotaskCheckpointLimitedOutcome::Completed) => {}
                        Ok(MicrotaskCheckpointLimitedOutcome::Stopped(reason)) => {
                          stop_reason = Some(reason);
                          return;
                        }
                        Err(err) => {
                          let progressed = run_state.microtasks_executed() != before;
                          if progressed {
                            log(&format!(
                              "JavaScript: uncaught exception in microtask checkpoint: {err}"
                            ));
                          } else {
                            fatal_error = Some(err);
                            return;
                          }
                        }
                      }
                    }
                  }
                  ScriptSchedulerAction::QueueTask { node_id, source_text, .. } => {
                    if source_text.as_bytes().len() > max_script_bytes {
                      log(&format!(
                        "JavaScript: skipping script task ({} bytes > max {})",
                        source_text.as_bytes().len(),
                        max_script_bytes
                      ));
                      continue;
                    }

                    let orchestrator = std::rc::Rc::clone(&orchestrator);
                    let js_execution_depth = std::rc::Rc::clone(&js_execution_depth);
                    let source_text: Arc<str> = Arc::from(source_text);
                    let name: Arc<str> = Arc::from(format!("<script {}>", node_id.index()));
                    if let Err(err) = event_loop.queue_task(TaskSource::Script, move |host, event_loop| {
                      let _guard = JsExecutionGuard::enter(&js_execution_depth);

                      struct Adapter<'a> {
                        name: Arc<str>,
                        source_text: Arc<str>,
                        event_loop: &'a mut EventLoop<WindowHostState>,
                        run_limits: RunLimits,
                      }
                      impl fastrender::js::ScriptBlockExecutor<WindowHostState> for Adapter<'_> {
                        fn execute_script(
                          &mut self,
                          host: &mut WindowHostState,
                          _orchestrator: &mut ScriptOrchestrator,
                          _script: fastrender::dom2::NodeId,
                          script_type: ScriptType,
                        ) -> Result<()> {
                          if script_type != ScriptType::Classic {
                            return Ok(());
                          }
                          {
                            let window = host.window_mut();
                            window.reset_interrupt();
                            window.vm_mut().set_budget(js_budget_for_script(self.run_limits));
                          }
                          let result = host.exec_script_with_name_in_event_loop(
                            self.event_loop,
                            self.name.clone(),
                            self.source_text.clone(),
                          );
                          {
                            let window = host.window_mut();
                            window
                              .vm_mut()
                              .set_budget(Budget::unlimited(DEFAULT_JS_CHECK_TIME_EVERY));
                          }
                          result.map(|_| ())
                        }
                      }

                      let mut adapter = Adapter {
                        name: name.clone(),
                        source_text: source_text.clone(),
                        event_loop,
                        run_limits,
                      };
                      let exec_result = orchestrator
                        .borrow_mut()
                        .execute_script_element(host, node_id, ScriptType::Classic, &mut adapter);
                      exec_result.map_err(|err| fastrender::Error::Other(format!("{}: {err}", &*name)))
                    }) {
                      log(&format!("JavaScript: failed to queue script task: {err}"));
                    } else {
                      scripts_queued += 1;
                    }
                  }
                  ScriptSchedulerAction::QueueScriptEventTask { .. } => {}
                }
              }

              for (script_id, node_id, url, destination) in start_fetches.drain(..) {
                if blocking.contains(&script_id) {
                  let charset_attr = host.dom().get_attribute(node_id, "charset").ok().flatten();
                  let fetched = match fetch_script_source(
                    fetcher.as_ref(),
                    &base_hint,
                    &url,
                    destination,
                    max_script_bytes,
                  ) {
                    Ok(fetched) => fetched,
                    Err(err) => {
                      log(&format!(
                        "JavaScript: failed to fetch blocking script {url} (continuing): {err}"
                      ));
                      actions = scheduler.borrow_mut().fetch_failed(script_id).unwrap_or_default();
                      continue 'apply_actions;
                    }
                  };

                  if max_script_bytes != usize::MAX && fetched.bytes.len() > max_script_bytes {
                    log(&format!(
                      "JavaScript: skipping external script {url} ({} bytes > max {})",
                      fetched.bytes.len(),
                      max_script_bytes
                    ));
                    actions = scheduler.borrow_mut().fetch_failed(script_id).unwrap_or_default();
                    continue 'apply_actions;
                  }

                  let source_text = decode_external_classic_script_bytes(
                    &fetched.bytes,
                    fetched.content_type.as_deref(),
                    charset_attr,
                  );
                  actions = scheduler.borrow_mut().fetch_completed(script_id, source_text).unwrap_or_default();
                  continue 'apply_actions;
                }

                let scheduler = std::rc::Rc::clone(&scheduler);
                let orchestrator = std::rc::Rc::clone(&orchestrator);
                let js_execution_depth = std::rc::Rc::clone(&js_execution_depth);
                let node_id_for_task = node_id;
                let url_for_task = url.clone();
                let destination_for_task = destination;
                let fetcher = fetcher.clone();
                if let Err(err) = event_loop.queue_task(TaskSource::Networking, move |host, event_loop| {
                  let charset_attr = host
                    .dom()
                    .get_attribute(node_id_for_task, "charset")
                    .ok()
                    .flatten();
                  let fetched = fetch_script_source(
                    fetcher.as_ref(),
                    &host.document_url,
                    &url_for_task,
                    destination_for_task,
                    max_script_bytes,
                  );
                  let actions = match fetched {
                    Ok(fetched) => {
                      if max_script_bytes != usize::MAX && fetched.bytes.len() > max_script_bytes {
                        scheduler.borrow_mut().fetch_failed(script_id)?
                      } else {
                        let source_text = decode_external_classic_script_bytes(
                          &fetched.bytes,
                          fetched.content_type.as_deref(),
                          charset_attr,
                        );
                        scheduler.borrow_mut().fetch_completed(script_id, source_text)?
                      }
                    }
                    Err(_err) => scheduler.borrow_mut().fetch_failed(script_id)?,
                  };

                  let mut queued: Vec<(ScriptId, fastrender::dom2::NodeId, String)> = Vec::new();
                  for action in actions {
                    match action {
                      ScriptSchedulerAction::QueueTask { node_id, source_text, .. } => {
                        queued.push((script_id, node_id, source_text));
                      }
                      ScriptSchedulerAction::ExecuteNow { node_id, source_text, .. } => {
                        let _guard = JsExecutionGuard::enter(&js_execution_depth);
                        struct Adapter<'a> {
                          name: Arc<str>,
                          source_text: Arc<str>,
                          event_loop: &'a mut EventLoop<WindowHostState>,
                          run_limits: RunLimits,
                        }
                        impl fastrender::js::ScriptBlockExecutor<WindowHostState> for Adapter<'_> {
                          fn execute_script(
                            &mut self,
                            host: &mut WindowHostState,
                            _orchestrator: &mut ScriptOrchestrator,
                            _script: fastrender::dom2::NodeId,
                            script_type: ScriptType,
                          ) -> Result<()> {
                            if script_type != ScriptType::Classic {
                              return Ok(());
                            }
                            {
                              let window = host.window_mut();
                              window.reset_interrupt();
                              window.vm_mut().set_budget(js_budget_for_script(self.run_limits));
                            }
                            let result = host.exec_script_with_name_in_event_loop(
                              self.event_loop,
                              self.name.clone(),
                              self.source_text.clone(),
                            );
                            {
                              let window = host.window_mut();
                              window
                                .vm_mut()
                                .set_budget(Budget::unlimited(DEFAULT_JS_CHECK_TIME_EVERY));
                            }
                            result.map(|_| ())
                          }
                        }
                        let name: Arc<str> = Arc::from(format!("<script {}>", node_id.index()));
                        let mut adapter = Adapter {
                          name: name.clone(),
                          source_text: Arc::from(source_text),
                          event_loop,
                          run_limits,
                        };
                        let _ = orchestrator
                          .borrow_mut()
                          .execute_script_element(host, node_id, ScriptType::Classic, &mut adapter);
                      }
                      _ => {}
                    }
                  }

                  for (_id, node_id, source_text) in queued {
                    let orchestrator = std::rc::Rc::clone(&orchestrator);
                    let js_execution_depth = std::rc::Rc::clone(&js_execution_depth);
                    let source_text: Arc<str> = Arc::from(source_text);
                    let name: Arc<str> = Arc::from(format!("<script {}>", node_id.index()));
                    event_loop.queue_task(TaskSource::Script, move |host, event_loop| {
                      let _guard = JsExecutionGuard::enter(&js_execution_depth);
                      struct Adapter<'a> {
                        name: Arc<str>,
                        source_text: Arc<str>,
                        event_loop: &'a mut EventLoop<WindowHostState>,
                        run_limits: RunLimits,
                      }
                      impl fastrender::js::ScriptBlockExecutor<WindowHostState> for Adapter<'_> {
                        fn execute_script(
                          &mut self,
                          host: &mut WindowHostState,
                          _orchestrator: &mut ScriptOrchestrator,
                          _script: fastrender::dom2::NodeId,
                          script_type: ScriptType,
                        ) -> Result<()> {
                          if script_type != ScriptType::Classic {
                            return Ok(());
                          }
                          {
                            let window = host.window_mut();
                            window.reset_interrupt();
                            window.vm_mut().set_budget(js_budget_for_script(self.run_limits));
                          }
                          let result = host.exec_script_with_name_in_event_loop(
                            self.event_loop,
                            self.name.clone(),
                            self.source_text.clone(),
                          );
                          {
                            let window = host.window_mut();
                            window
                              .vm_mut()
                              .set_budget(Budget::unlimited(DEFAULT_JS_CHECK_TIME_EVERY));
                          }
                          result.map(|_| ())
                        }
                      }
                      let mut adapter = Adapter {
                        name: name.clone(),
                        source_text: source_text.clone(),
                        event_loop,
                        run_limits,
                      };
                      orchestrator
                        .borrow_mut()
                        .execute_script_element(host, node_id, ScriptType::Classic, &mut adapter)
                        .map_err(|err| fastrender::Error::Other(format!("{}: {err}", &*name)))
                    })?;
                  }

                  Ok(())
                }) {
                  log(&format!("JavaScript: failed to queue script fetch task: {err}"));
                } else {
                  scripts_queued += 1;
                }
              }

              break;
            }
          });

          if let Some(err) = fatal_error {
            js_active = false;
            event_loop.clear_all_pending_work();
            log(&format!("JavaScript: error while running JS (continuing): {err}"));
          } else if let Some(reason) = stop_reason {
            js_active = false;
            event_loop.clear_all_pending_work();
            match reason {
              RunUntilIdleStopReason::MaxTasks { executed, limit } => {
                log(&format!(
                  "JavaScript: stopped after max tasks (executed {executed} / limit {limit})"
                ));
              }
              RunUntilIdleStopReason::MaxMicrotasks { executed, limit } => {
                log(&format!(
                  "JavaScript: stopped after max microtasks (executed {executed} / limit {limit})"
                ));
              }
              RunUntilIdleStopReason::WallTime { elapsed, limit } => {
                log(&format!(
                  "JavaScript: stopped after wall time (elapsed {}ms / limit {}ms)",
                  elapsed.as_millis(),
                  limit.as_millis()
                ));
              }
            }
          }

          let updated = host.dom().clone_with_events();
          if let Some(mut doc) = parser.document_mut() {
            *doc = updated;
          }

          if js_active && !executed_sync_script && !event_loop.is_idle() {
            match parser.with_active_document_write(|| {
              run_event_loop_until_idle_limited_handling_errors(
                &mut event_loop,
                &mut host,
                &mut run_state,
                &mut log,
              )
            }) {
              Ok(RunUntilIdleOutcome::Idle) => {}
              Ok(RunUntilIdleOutcome::Stopped(reason)) => {
                js_active = false;
                event_loop.clear_all_pending_work();
                match reason {
                  RunUntilIdleStopReason::MaxTasks { executed, limit } => {
                    log(&format!(
                      "JavaScript: stopped after max tasks (executed {executed} / limit {limit})"
                    ));
                  }
                  RunUntilIdleStopReason::MaxMicrotasks { executed, limit } => {
                    log(&format!(
                      "JavaScript: stopped after max microtasks (executed {executed} / limit {limit})"
                    ));
                  }
                  RunUntilIdleStopReason::WallTime { elapsed, limit } => {
                    log(&format!(
                      "JavaScript: stopped after wall time (elapsed {}ms / limit {}ms)",
                      elapsed.as_millis(),
                      limit.as_millis()
                    ));
                  }
                }
              }
              Err(err) => {
                js_active = false;
                event_loop.clear_all_pending_work();
                log(&format!("JavaScript: error while running event loop (continuing): {err}"));
              }
            }

            let updated = host.dom().clone_with_events();
            if let Some(mut doc) = parser.document_mut() {
              *doc = updated;
            }
          }
        }
        Ok(StreamingParserYield::NeedMoreInput) => {
          log("JavaScript: streaming HTML parser requested more input after EOF (continuing without JS)");
          js_active = false;
          event_loop.clear_all_pending_work();
          break;
        }
        Ok(StreamingParserYield::Finished { document }) => {
          let base_url = parser.current_base_url();
          host.base_url = base_url.clone();
          host.window_mut().set_base_url(base_url);
          host.mutate_dom(|dom| {
            *dom = document;
            ((), true)
          });
          break;
        }
        Err(err) => {
          log(&format!("JavaScript: failed to parse HTML (continuing without JS): {err}"));
          return render_fetched_document(renderer, &resource, Some(&base_hint), &options);
        }
      }
    }

    if js_active {
      // Signal parsing completion to allow deferred scripts to run once fetched.
      let actions = match scheduler.borrow_mut().parsing_completed() {
        Ok(actions) => actions,
        Err(err) => {
          log(&format!("JavaScript: scheduler error at parsing completion (continuing): {err}"));
          Vec::new()
        }
      };
      for action in actions {
        if let ScriptSchedulerAction::QueueTask { node_id, source_text, .. } = action {
          let orchestrator = std::rc::Rc::clone(&orchestrator);
          let js_execution_depth = std::rc::Rc::clone(&js_execution_depth);
          let source_text: Arc<str> = Arc::from(source_text);
          let name: Arc<str> = Arc::from(format!("<script {}>", node_id.index()));
          if let Err(err) = event_loop.queue_task(TaskSource::Script, move |host, event_loop| {
            let _guard = JsExecutionGuard::enter(&js_execution_depth);
            struct Adapter<'a> {
              name: Arc<str>,
              source_text: Arc<str>,
              event_loop: &'a mut EventLoop<WindowHostState>,
              run_limits: RunLimits,
            }
            impl fastrender::js::ScriptBlockExecutor<WindowHostState> for Adapter<'_> {
              fn execute_script(
                &mut self,
                host: &mut WindowHostState,
                _orchestrator: &mut ScriptOrchestrator,
                _script: fastrender::dom2::NodeId,
                script_type: ScriptType,
              ) -> Result<()> {
                if script_type != ScriptType::Classic {
                  return Ok(());
                }
                {
                  let window = host.window_mut();
                  window.reset_interrupt();
                  window.vm_mut().set_budget(js_budget_for_script(self.run_limits));
                }
                let result = host.exec_script_with_name_in_event_loop(
                  self.event_loop,
                  self.name.clone(),
                  self.source_text.clone(),
                );
                {
                  let window = host.window_mut();
                  window
                    .vm_mut()
                    .set_budget(Budget::unlimited(DEFAULT_JS_CHECK_TIME_EVERY));
                }
                result.map(|_| ())
              }
            }
            let mut adapter = Adapter {
              name: name.clone(),
              source_text: source_text.clone(),
              event_loop,
              run_limits,
            };
            orchestrator
              .borrow_mut()
              .execute_script_element(host, node_id, ScriptType::Classic, &mut adapter)
              .map_err(|err| fastrender::Error::Other(format!("{}: {err}", &*name)))
          }) {
            log(&format!("JavaScript: failed to queue deferred script task: {err}"));
          } else {
            scripts_queued += 1;
          }
        }
      }
    }

    if scripts_queued > 0 {
      log(&format!(
        "JavaScript: queued {scripts_queued} task(s) (max_tasks={} max_microtasks={} max_wall_ms={} max_script_bytes={})",
        run_limits.max_tasks, run_limits.max_microtasks, js.max_wall_ms, max_script_bytes
      ));
    }

    if js_active {
      match run_event_loop_until_idle_limited_handling_errors(
        &mut event_loop,
        &mut host,
        &mut run_state,
        &mut log,
      ) {
        Ok(RunUntilIdleOutcome::Idle) => {}
        Ok(RunUntilIdleOutcome::Stopped(reason)) => match reason {
          RunUntilIdleStopReason::MaxTasks { executed, limit } => {
            log(&format!(
              "JavaScript: stopped after max tasks (executed {executed} / limit {limit})"
            ));
          }
          RunUntilIdleStopReason::MaxMicrotasks { executed, limit } => {
            log(&format!(
              "JavaScript: stopped after max microtasks (executed {executed} / limit {limit})"
            ));
          }
          RunUntilIdleStopReason::WallTime { elapsed, limit } => {
            log(&format!(
              "JavaScript: stopped after wall time (elapsed {}ms / limit {}ms)",
              elapsed.as_millis(),
              limit.as_millis()
            ));
          }
        },
        Err(err) => {
          log(&format!("JavaScript: error while running event loop (continuing): {err}"));
        }
      }
    }

    // If a render timeout is configured, share the remaining budget with the renderer so JS + render
    // stays bounded by a single deadline.
    if options.timeout.is_some() {
      if let Some(remaining) = deadline.remaining_timeout() {
        options.timeout = Some(remaining);
      } else {
        options.timeout = Some(Duration::from_millis(0));
      }
    }

    // Snapshot mutated dom2 tree back into renderer DOM and render one frame.
    let dom = host.dom().to_renderer_dom();
    let report = match renderer.prepare_dom_with_options(dom, Some(&base_hint), options.clone()) {
      Ok(report) => report,
      Err(err) => {
        log(&format!(
          "JavaScript: failed to prepare mutated DOM (falling back to static render): {err}"
        ));
        return render_fetched_document(renderer, &resource, Some(&base_hint), &options);
      }
    };

    Ok(fastrender::api::RenderResult {
      pixmap: report.document.paint_default()?,
      accessibility: None,
      diagnostics: report.diagnostics,
    })
  })?;
  log_diagnostics(&render_result.diagnostics, &mut log);

  let image_data = encode_image(&render_result.pixmap, output_format)?;
  std::fs::write(output, &image_data)?;

  println!("✓ Successfully rendered {url} to {}", output.display());
  println!("  Image size: {} bytes", image_data.len());
  Ok(())
}

fn try_main(args: Args) -> Result<()> {
  if args.memory.mem_limit_mb > 0 {
    match fastrender::process_limits::apply_address_space_limit_mb(args.memory.mem_limit_mb) {
      Ok(fastrender::process_limits::AddressSpaceLimitStatus::Applied) => {}
      Ok(fastrender::process_limits::AddressSpaceLimitStatus::Unsupported) => {
        eprintln!(
          "warning: --mem-limit-mb is only supported on Linux; ignoring (requested {} MiB)",
          args.memory.mem_limit_mb
        );
      }
      Ok(fastrender::process_limits::AddressSpaceLimitStatus::Disabled) => {}
      Err(err) => {
        return Err(fastrender::Error::Other(format!(
          "failed to apply --mem-limit-mb {}: {err}",
          args.memory.mem_limit_mb
        )));
      }
    }
  }

  let media_prefs = MediaPreferences::from(&args.media.prefs);
  media_prefs.apply_env();
  if args.full_page {
    std::env::set_var("FASTR_FULL_PAGE", "1");
  }
  if args.timings {
    std::env::set_var("FASTR_RENDER_TIMINGS", "1");
  }

  // Resolve dimensions from viewport or deprecated positional args
  let (viewport_w, viewport_h) = args.surface.viewport;
  let width = args.width.unwrap_or(viewport_w);
  let height = args.height.unwrap_or(viewport_h);
  let scroll_x = args.scroll_x_pos.unwrap_or(args.scroll_x) as f32;
  let scroll_y = args.scroll_y_pos.unwrap_or(args.scroll_y) as f32;

  let output_ext = args.output_format.extension();
  let output_format = args.output_format.output_format();
  let output = args
    .output
    .unwrap_or_else(|| format!("{}.{}", url_to_filename(&args.url), output_ext));

  let output_path = Path::new(&output);
  if let Some(parent) = output_path.parent() {
    if !parent.as_os_str().is_empty() {
      std::fs::create_dir_all(parent)?;
    }
  }

  let banner = format!(
    "User-Agent: {}\nAccept-Language: {}\nViewport: {}x{} @{}x, scroll ({}, {})\nOutput: {} ({})",
    normalize_user_agent_for_log(&args.user_agent),
    args.accept_language,
    width,
    height,
    args.surface.dpr,
    scroll_x,
    scroll_y,
    output,
    output_ext
  );
  #[cfg(feature = "disk_cache")]
  let banner = {
    let max_age = if args.disk_cache.max_age_secs == 0 {
      "none".to_string()
    } else {
      format!("{}s", args.disk_cache.max_age_secs)
    };
    let mut banner = banner;
    banner.push_str(&format!(
      "\nDisk cache: dir={} max_bytes={} max_age={}",
      args.cache_dir.display(),
      args.disk_cache.max_bytes,
      max_age
    ));
    banner
  };
  eprintln!("{banner}");

  let timeout_secs = args.timeout.seconds(Some(0));
  let soft_timeout_ms = timeout_secs
    .map(Duration::from_secs)
    .and_then(|hard_timeout| compute_soft_timeout_ms(hard_timeout, args.soft_timeout_ms));

  #[cfg(feature = "disk_cache")]
  {
    // Keep parity with render_pages/pageset_progress: ensure disk cache is ready up front.
    std::fs::create_dir_all(&args.cache_dir)?;
  }

  let RenderConfigBundle {
    config,
    mut options,
  } = build_render_configs(&RenderSurface {
    viewport: (width, height),
    scroll_x,
    scroll_y,
    dpr: args.surface.dpr,
    media_type: args.media.media_type(),
    render_parse_scripting_enabled: args.render_parse.render_parse_scripting_enabled,
    animation_time_ms: args.animation_time.animation_time_ms(),
    css_limit: args.css_limit,
    allow_partial: args.allow_partial.allow_partial,
    apply_meta_viewport: true,
    base_url: args.base_url.base_url.clone(),
    allow_file_from_http: args.resource_access.allow_file_from_http,
    block_mixed_content: args.resource_access.block_mixed_content,
    same_origin_subresources: args.resource_access.same_origin_subresources,
    allowed_subresource_origins: args.resource_access.allow_subresource_origin.clone(),
    trace_output: args.trace_out.clone(),
    layout_parallelism: args.layout_parallel.parallelism(),
    font_config: None,
    compat_profile: args.compat.compat_profile(),
    dom_compat_mode: args.compat.dom_compat_mode(),
  });
  if args.memory.stage_mem_budget_mb > 0 {
    let bytes = args
      .memory
      .stage_mem_budget_mb
      .checked_mul(1024 * 1024)
      .ok_or_else(|| {
        fastrender::Error::Other(format!(
          "--stage-mem-budget-mb is too large: {} MiB",
          args.memory.stage_mem_budget_mb
        ))
      })?;
    options.stage_mem_budget_bytes = Some(bytes);
  }
  if let Some(ms) = soft_timeout_ms {
    if ms > 0 {
      options.timeout = Some(Duration::from_millis(ms));
    }
  }

  let http = build_http_fetcher(
    &args.user_agent,
    &args.accept_language,
    timeout_secs.map(Duration::from_secs),
  );
  let honor_http_freshness = cfg!(feature = "disk_cache") && !args.no_http_freshness;
  let memory_config = CachingFetcherConfig {
    honor_http_cache_freshness: honor_http_freshness,
    ..CachingFetcherConfig::default()
  };
  #[cfg(feature = "disk_cache")]
  let mut disk_config = args.disk_cache.to_config();
  #[cfg(feature = "disk_cache")]
  {
    disk_config.namespace = Some(common::render_pipeline::disk_cache_namespace(
      &args.user_agent,
      &args.accept_language,
    ));
  }
  #[cfg(feature = "disk_cache")]
  let fetcher: Arc<dyn ResourceFetcher> = Arc::new(DiskCachingFetcher::with_configs(
    http,
    &args.cache_dir,
    memory_config,
    disk_config,
  ));
  #[cfg(not(feature = "disk_cache"))]
  let fetcher: Arc<dyn ResourceFetcher> =
    Arc::new(CachingFetcher::with_config(http, memory_config));

  let render_pool = FastRenderPool::with_config(
    FastRenderPoolConfig::new()
      .with_renderer_config(config.clone())
      .with_fetcher(Arc::clone(&fetcher))
      .with_pool_size(1),
  )?;

  let (tx, rx) = channel();
  let url_clone = args.url.clone();
  let output_path = output_path.to_path_buf();
  let base_url_override = args.base_url.base_url.clone();
  let output_format = output_format;
  let render_pool = render_pool.clone();
  let fetcher = Arc::clone(&fetcher);
  let mut js_options = args.js.to_options();
  // Preserve this binary's conservative defaults unless the user explicitly overrode them.
  if args.js.max_tasks_per_spin.is_none() {
    js_options.event_loop_run_limits.max_tasks = DEFAULT_JS_MAX_TASKS;
  }
  if args.js.max_microtasks_per_spin.is_none() {
    js_options.event_loop_run_limits.max_microtasks = DEFAULT_JS_MAX_MICROTASKS;
  }
  if args.js.max_wall_time_per_spin_ms.is_none() {
    js_options.event_loop_run_limits.max_wall_time =
      (DEFAULT_JS_MAX_WALL_MS > 0).then(|| Duration::from_millis(DEFAULT_JS_MAX_WALL_MS));
  }
  if args.js.max_script_bytes.is_none() {
    js_options.max_script_bytes = DEFAULT_JS_MAX_SCRIPT_BYTES;
  }

  let js = JsCliConfig {
    enabled: args.js_enabled,
    max_tasks: js_options.event_loop_run_limits.max_tasks,
    max_microtasks: js_options.event_loop_run_limits.max_microtasks,
    max_wall_ms: js_options
      .event_loop_run_limits
      .max_wall_time
      .map(|d| d.as_millis().try_into().unwrap_or(u64::MAX))
      .unwrap_or(0),
    max_script_bytes: js_options.max_script_bytes,
  };

  thread::Builder::new()
    .name("fetch_and_render-worker".to_string())
    .stack_size(CLI_RENDER_STACK_SIZE)
    .spawn(move || {
      let res = render_page(
        &url_clone,
        &output_path,
        RenderConfigBundle { config, options },
        render_pool,
        fetcher,
        output_format,
        base_url_override,
        js,
      );
      let _ = tx.send(res);
    })?;

  if let Some(secs) = timeout_secs {
    match rx.recv_timeout(Duration::from_secs(secs)) {
      Ok(res) => res,
      Err(RecvTimeoutError::Timeout) => {
        eprintln!("Render timed out after {secs} seconds");
        std::process::exit(1);
      }
      Err(RecvTimeoutError::Disconnected) => {
        eprintln!("Render worker exited unexpectedly");
        std::process::exit(1);
      }
    }
  } else {
    match rx.recv() {
      Ok(res) => res,
      Err(_) => {
        eprintln!("Render worker exited unexpectedly");
        std::process::exit(1);
      }
    }
  }
}

fn main() {
  let args = Args::parse();
  let verbose = args.verbose;
  if let Err(err) = try_main(args) {
    eprintln!("{}", format_error_with_chain(&err, verbose));
    if !verbose && err.source().is_some() {
      eprintln!("note: re-run with --verbose to see full error context");
    }
    std::process::exit(1);
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use fastrender::resource::FetchedResource;
  use std::sync::Mutex;

  #[derive(Default)]
  struct DestRecordingFetcher {
    destinations: Mutex<Vec<FetchDestination>>,
  }

  impl ResourceFetcher for DestRecordingFetcher {
    fn fetch(&self, url: &str) -> Result<FetchedResource> {
      Ok(FetchedResource::with_final_url(
        b"console.log(1)".to_vec(),
        Some("text/javascript".to_string()),
        Some(url.to_string()),
      ))
    }

    fn fetch_with_request(&self, req: FetchRequest<'_>) -> Result<FetchedResource> {
      self.destinations.lock().unwrap().push(req.destination);
      self.fetch(req.url)
    }
  }

  #[test]
  fn fetch_script_source_uses_script_destinations() {
    let fetcher = DestRecordingFetcher::default();
    let _ = fetch_script_source(
      &fetcher,
      "https://example.com/index.html",
      "https://example.com/a.js",
      FetchDestination::Script,
      1024,
    )
    .unwrap();
    let _ = fetch_script_source(
      &fetcher,
      "https://example.com/index.html",
      "https://example.com/b.js",
      FetchDestination::ScriptCors,
      1024,
    )
    .unwrap();
    assert_eq!(
      *fetcher.destinations.lock().unwrap(),
      vec![FetchDestination::Script, FetchDestination::ScriptCors]
    );
  }
}
