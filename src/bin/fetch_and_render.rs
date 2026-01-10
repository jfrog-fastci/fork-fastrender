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
  LayoutParallelArgs, MediaArgs, MemoryGuardArgs, OutputFormatArgs, ResourceAccessArgs, TimeoutArgs,
  ViewportArgs,
};
use common::media_prefs::MediaPreferences;
use common::render_pipeline::{
  build_http_fetcher, build_render_configs, compute_soft_timeout_ms,
  follow_client_redirects_resource, format_error_with_chain, log_diagnostics, read_cached_document,
  render_fetched_document, RenderConfigBundle, RenderSurface, CLI_RENDER_STACK_SIZE,
};
use encoding_rs::{Encoding, UTF_8};
use fastrender::api::{FastRenderPool, FastRenderPoolConfig};
use fastrender::dom::DomParseOptions;
use fastrender::dom2::{Document as Dom2Document, NodeKind as Dom2NodeKind};
use fastrender::html::base_url_tracker::BaseUrlTracker;
use fastrender::image_output::encode_image;
use fastrender::js::{
  determine_script_type_dom2, CurrentScriptHost, EventLoop, RunLimits, RunUntilIdleOutcome,
  RunUntilIdleStopReason, ScriptType, TaskSource, WindowHostState, ModuleGraphLoader,
};
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
  let render_remaining = fastrender::render_control::root_deadline()
    .and_then(|deadline| deadline.remaining_timeout());

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

  if let Some(label) = charset_attr.map(trim_ascii_whitespace).filter(|v| !v.is_empty()) {
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

fn queue_classic_script_task(
  event_loop: &mut EventLoop<WindowHostState>,
  run_limits: RunLimits,
  script_node: fastrender::dom2::NodeId,
  script_name: Arc<str>,
  script_text: Arc<str>,
) -> Result<()> {
  let script_node_for_task = script_node;
  let script_name_for_task = script_name.clone();
  let script_text_for_task = script_text.clone();
  event_loop.queue_task(TaskSource::Script, move |host, event_loop| {
    let prev = host.current_script_state().borrow().current_script;
    host.current_script_state().borrow_mut().current_script = Some(script_node_for_task);

    // Execute scripts through the `WindowHostState` helper so Promise jobs are routed into the
    // host-owned HTML-like event loop microtask queue.
    {
      let window = host.window_mut();
      window.reset_interrupt();
      window.vm_mut().set_budget(js_budget_for_script(run_limits));
    }
    let result = host.exec_script_with_name_in_event_loop(
      event_loop,
      script_name_for_task.clone(),
      script_text_for_task.clone(),
    );
    {
      let window = host.window_mut();
      window
        .vm_mut()
        .set_budget(Budget::unlimited(DEFAULT_JS_CHECK_TIME_EVERY));
    }

    host.current_script_state().borrow_mut().current_script = prev;

    result
      .map(|_| ())
      .map_err(|err| fastrender::Error::Other(format!("{}: {err}", &*script_name_for_task)))
  })?;
  Ok(())
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
  let RenderConfigBundle { config, options } = bundle;
  let dom_compat_mode = config.dom_compat_mode;
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

    // Ensure JavaScript execution is bounded by the same cooperative render timeout.
    // We keep this deadline alive through the render so check_root-based operations see it too.
    let deadline = RenderDeadline::new(options.timeout, options.cancel_callback.clone());
    let _deadline_guard = DeadlineGuard::install(Some(&deadline));

    // Decode + parse the HTML with scripting-enabled semantics (affects <noscript> parsing).
    let html = fastrender::html::encoding::decode_html_bytes(
      &resource.bytes,
      resource.content_type.as_deref(),
    );
    let dom = match fastrender::dom::parse_html_with_options(
      &html,
      DomParseOptions {
        scripting_enabled: true,
        compatibility_mode: dom_compat_mode,
      },
    ) {
      Ok(dom) => dom,
      Err(err) => {
        log(&format!("JavaScript: failed to parse HTML (continuing without JS): {err}"));
        return render_fetched_document(renderer, &resource, Some(&base_hint), &options);
      }
    };

    // Import into dom2 for mutation.
    let dom2 = Dom2Document::from_renderer_dom(&dom);
    let mut host = match WindowHostState::new_with_fetcher(
      dom2,
      base_hint.clone(),
      fetcher.clone(),
    ) {
      Ok(host) => host,
      Err(err) => {
        log(&format!(
          "JavaScript: failed to initialize window host (continuing without JS): {err}"
        ));
        return render_fetched_document(renderer, &resource, Some(&base_hint), &options);
      }
    };

    // Collect and execute inline scripts best-effort.
    let run_limits = js.run_limits();
    let max_script_bytes = if js.max_script_bytes == 0 {
      usize::MAX
    } else {
      js.max_script_bytes
    };
    let mut event_loop = EventLoop::<WindowHostState>::new();
    let mut scripts_queued = 0usize;
    let module_loader = Rc::new(RefCell::new(ModuleGraphLoader::new(fetcher.clone())));

    #[derive(Clone, Copy)]
    struct DomScanState {
      in_head: bool,
      in_foreign_namespace: bool,
      in_template: bool,
      in_shadow_root: bool,
    }

    let dom = host.dom();
    let mut base_url_tracker = BaseUrlTracker::new(Some(&base_hint));
    let mut stack: Vec<(fastrender::dom2::NodeId, DomScanState)> = Vec::new();
    stack.push((
      dom.root(),
      DomScanState {
        in_head: false,
        in_foreign_namespace: false,
        in_template: false,
        in_shadow_root: false,
      },
    ));

    while let Some((node_id, state)) = stack.pop() {
      let node = dom.node(node_id);

      let (next_in_head, next_in_foreign_namespace, next_in_shadow_root) = match &node.kind {
        Dom2NodeKind::Element {
          tag_name,
          namespace,
          attributes,
        } => {
          // Track parse-time base URL updates best-effort in document order.
          base_url_tracker.on_element_inserted(
            tag_name,
            namespace,
            attributes,
            state.in_head,
            state.in_foreign_namespace,
            state.in_template || state.in_shadow_root,
          );

          // Execute HTML <script> elements (classic + module).
          if tag_name.eq_ignore_ascii_case("script")
            && (namespace.is_empty() || namespace == fastrender::dom::HTML_NAMESPACE)
            && dom.is_connected_for_scripting(node_id)
          {
            match determine_script_type_dom2(dom, node_id) {
              ScriptType::Classic => {
                let src_attr_present = dom.has_attribute(node_id, "src").unwrap_or(false);
                if src_attr_present {
                  // HTML semantics: presence of `src` suppresses inline execution, even if empty/invalid.
                  let raw_src = dom.get_attribute(node_id, "src").ok().flatten().unwrap_or("");
                  if let Some(resolved_src) = base_url_tracker.resolve_script_src(raw_src) {
                    let charset_attr = dom.get_attribute(node_id, "charset").ok().flatten();
                    let req = FetchRequest::new(resolved_src.as_str(), FetchDestination::Other)
                      .with_referrer_url(base_hint.as_str());

                    let fetched = if max_script_bytes == usize::MAX {
                      fetcher.fetch_with_request(req)
                    } else {
                      // Fetch at most `max_script_bytes + 1` so we can deterministically decide whether
                      // the script exceeds the configured budget without downloading arbitrary amounts.
                      fetcher.fetch_partial_with_request(req, max_script_bytes.saturating_add(1))
                    };

                    match fetched {
                      Ok(fetched) => {
                        if max_script_bytes != usize::MAX && fetched.bytes.len() > max_script_bytes {
                          log(&format!(
                            "JavaScript: skipping external script ({} bytes > max {}) ({resolved_src})",
                            fetched.bytes.len(),
                            max_script_bytes
                          ));
                        } else {
                          let script_text = decode_external_classic_script_bytes(
                            &fetched.bytes,
                            fetched.content_type.as_deref(),
                            charset_attr,
                          );
                          let script_name: Arc<str> =
                            Arc::from(fetched.final_url.clone().unwrap_or(resolved_src));
                          let script_text: Arc<str> = Arc::from(script_text);

                          if let Err(err) = queue_classic_script_task(
                            &mut event_loop,
                            run_limits,
                            node_id,
                            script_name,
                            script_text,
                          ) {
                            log(&format!("JavaScript: failed to queue script task: {err}"));
                            break;
                          }
                          scripts_queued += 1;
                        }
                      }
                      Err(err) => {
                        log(&format!(
                          "JavaScript: failed to fetch external script {resolved_src} (continuing): {err}"
                        ));
                        // Fetch failures should not crash the render; skip and keep going.
                      }
                    }
                  } else {
                    log("JavaScript: skipping <script src> with empty/invalid/unresolvable src");
                  }
                } else {
                  // Inline classic script.
                  let mut inline_text = String::new();
                  for &child_id in &node.children {
                    if let Dom2NodeKind::Text { content } = &dom.node(child_id).kind {
                      inline_text.push_str(content);
                    }
                  }

                  if inline_text.as_bytes().len() > max_script_bytes {
                    log(&format!(
                      "JavaScript: skipping inline script ({} bytes > max {})",
                      inline_text.as_bytes().len(),
                      max_script_bytes
                    ));
                  } else {
                    let script_name: Arc<str> =
                      Arc::from(format!("<inline script {}>", node_id.index()));
                    let script_text: Arc<str> = Arc::from(inline_text);

                    if let Err(err) = queue_classic_script_task(
                      &mut event_loop,
                      run_limits,
                      node_id,
                      script_name,
                      script_text,
                    ) {
                      log(&format!("JavaScript: failed to queue script task: {err}"));
                      break;
                    }
                    scripts_queued += 1;
                  }
                }
              }
              ScriptType::Module => {
                let src_attr_present = dom.has_attribute(node_id, "src").unwrap_or(false);
                if src_attr_present {
                  let raw_src = dom.get_attribute(node_id, "src").ok().flatten().unwrap_or("");
                  if let Some(resolved_src) = base_url_tracker.resolve_script_src(raw_src) {
                    let loader = Rc::clone(&module_loader);
                    let entry_url: Arc<str> = Arc::from(resolved_src.clone());
                    let script_name: Arc<str> =
                      Arc::from(format!("<module script {}>", resolved_src));
                    if let Err(err) = event_loop.queue_task(TaskSource::Script, move |host, event_loop| {
                      let prev = host.current_script_state().borrow().current_script;
                      host.current_script_state().borrow_mut().current_script = None;

                      let bundle = loader
                        .borrow_mut()
                        .build_bundle_for_url(&entry_url, max_script_bytes)?;
                      let bundle_text: Arc<str> = Arc::from(bundle);

                      {
                        let window = host.window_mut();
                        window.reset_interrupt();
                        window.vm_mut().set_budget(js_budget_for_script(run_limits));
                      }
                      let result = host.exec_script_with_name_in_event_loop(
                        event_loop,
                        script_name.clone(),
                        bundle_text.clone(),
                      );
                      {
                        let window = host.window_mut();
                        window
                          .vm_mut()
                          .set_budget(Budget::unlimited(DEFAULT_JS_CHECK_TIME_EVERY));
                      }

                      host.current_script_state().borrow_mut().current_script = prev;

                      result
                        .map(|_| ())
                        .map_err(|err| fastrender::Error::Other(format!("{}: {err}", &*script_name)))
                    }) {
                      log(&format!("JavaScript: failed to queue module script task: {err}"));
                      break;
                    }
                    scripts_queued += 1;
                  } else {
                    log("JavaScript: skipping <script type=module src> with empty/invalid/unresolvable src");
                  }
                } else {
                  // Inline module script.
                  let mut inline_text = String::new();
                  for &child_id in &node.children {
                    if let Dom2NodeKind::Text { content } = &dom.node(child_id).kind {
                      inline_text.push_str(content);
                    }
                  }

                  if inline_text.as_bytes().len() > max_script_bytes {
                    log(&format!(
                      "JavaScript: skipping inline module script ({} bytes > max {})",
                      inline_text.as_bytes().len(),
                      max_script_bytes
                    ));
                  } else {
                    let loader = Rc::clone(&module_loader);
                    let inline_id: Arc<str> =
                      Arc::from(format!("{base_hint}#inline-module-{}", node_id.index()));
                    let inline_base_url: Arc<str> = Arc::from(
                      base_url_tracker
                        .current_base_url()
                        .unwrap_or_else(|| base_hint.clone()),
                    );
                    let script_name: Arc<str> =
                      Arc::from(format!("<inline module script {}>", node_id.index()));
                    let script_text: Arc<str> = Arc::from(inline_text);
                    if let Err(err) = event_loop.queue_task(TaskSource::Script, move |host, event_loop| {
                      let prev = host.current_script_state().borrow().current_script;
                      host.current_script_state().borrow_mut().current_script = None;

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
                        bundle_text.clone(),
                      );
                      {
                        let window = host.window_mut();
                        window
                          .vm_mut()
                          .set_budget(Budget::unlimited(DEFAULT_JS_CHECK_TIME_EVERY));
                      }

                      host.current_script_state().borrow_mut().current_script = prev;

                      result
                        .map(|_| ())
                        .map_err(|err| fastrender::Error::Other(format!("{}: {err}", &*script_name)))
                    }) {
                      log(&format!("JavaScript: failed to queue module script task: {err}"));
                      break;
                    }
                    scripts_queued += 1;
                  }
                }
              }
              ScriptType::ImportMap => {
                log("JavaScript: skipping <script type=importmap> (not supported yet)");
              }
              ScriptType::Unknown => {
                log("JavaScript: skipping <script> (unsupported type)");
              }
            }
          }

          let is_head = tag_name.eq_ignore_ascii_case("head")
            && (namespace.is_empty() || namespace == fastrender::dom::HTML_NAMESPACE);
          let next_in_head = state.in_head || is_head;
          let next_in_foreign_namespace = state.in_foreign_namespace
            || !(namespace.is_empty() || namespace == fastrender::dom::HTML_NAMESPACE);
          (next_in_head, next_in_foreign_namespace, state.in_shadow_root)
        }
        Dom2NodeKind::ShadowRoot { .. } => (
          state.in_head,
          state.in_foreign_namespace,
          /* next_in_shadow_root */ true,
        ),
        _ => (state.in_head, state.in_foreign_namespace, state.in_shadow_root),
      };

      let next_in_template = state.in_template || node.inert_subtree;

      let next_state = DomScanState {
        in_head: next_in_head,
        in_foreign_namespace: next_in_foreign_namespace,
        in_template: next_in_template,
        in_shadow_root: next_in_shadow_root,
      };

      // Push children in reverse so we traverse left-to-right in document order.
      for &child in node.children.iter().rev() {
        stack.push((child, next_state));
      }
    }

    if scripts_queued > 0 {
      log(&format!(
        "JavaScript: queued {scripts_queued} task(s) (max_tasks={} max_microtasks={} max_wall_ms={} max_script_bytes={})",
        run_limits.max_tasks, run_limits.max_microtasks, js.max_wall_ms, max_script_bytes
      ));
      match event_loop.run_until_idle_handling_errors(&mut host, run_limits, |err| {
        log(&format!("JavaScript: uncaught exception: {err}"));
      }) {
        Ok(RunUntilIdleOutcome::Idle) => {}
        Ok(RunUntilIdleOutcome::Stopped(reason)) => match reason {
          RunUntilIdleStopReason::MaxTasks { executed, limit } => {
            log(&format!("JavaScript: stopped after max tasks (executed {executed} / limit {limit})"));
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
