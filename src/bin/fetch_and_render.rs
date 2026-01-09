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
use fastrender::api::{FastRenderPool, FastRenderPoolConfig};
use fastrender::dom::DomParseOptions;
use fastrender::dom2::{Document as Dom2Document, NodeId as Dom2NodeId, NodeKind as Dom2NodeKind};
use fastrender::image_output::encode_image;
use fastrender::js::{
  EventLoop, RunLimits, RunUntilIdleOutcome, RunUntilIdleStopReason, TaskSource,
};
use fastrender::render_control::{DeadlineGuard, RenderDeadline};
use fastrender::resource::normalize_user_agent_for_log;
use fastrender::resource::url_to_filename;
#[cfg(not(feature = "disk_cache"))]
use fastrender::resource::CachingFetcher;
use fastrender::resource::CachingFetcherConfig;
#[cfg(feature = "disk_cache")]
use fastrender::resource::DiskCachingFetcher;
use fastrender::resource::FetchRequest;
use fastrender::resource::ResourceFetcher;
use fastrender::resource::DEFAULT_ACCEPT_LANGUAGE;
use fastrender::resource::DEFAULT_USER_AGENT;
use fastrender::OutputFormat;
use fastrender::Result;
use regex::Regex;
use std::error::Error;
use std::path::{Path, PathBuf};
use std::sync::mpsc::channel;
use std::sync::mpsc::RecvTimeoutError;
use std::sync::Arc;
use std::sync::OnceLock;
use std::thread;
use std::time::Duration;
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

#[derive(Debug)]
struct JsHost {
  dom: Dom2Document,
  diagnostics: Vec<String>,
}

impl JsHost {
  fn new(dom: Dom2Document) -> Self {
    Self {
      dom,
      diagnostics: Vec::new(),
    }
  }
}

fn apply_js_stub(host: &mut JsHost, script_text: &str) {
  let Some(class_name) = extract_document_element_class_name(script_text) else {
    let preview = script_text.trim().chars().take(120).collect::<String>();
    if !preview.is_empty() {
      host
        .diagnostics
        .push(format!("ignored unsupported script: {preview}"));
    }
    return;
  };

  let Some(doc_el) = find_document_element(&host.dom) else {
    host
      .diagnostics
      .push("document.documentElement is missing; cannot apply className assignment".to_string());
    return;
  };

  match dom2_set_attribute(&mut host.dom, doc_el, "class", &class_name) {
    Ok(true) => host.diagnostics.push(format!(
      "applied documentElement.className = {class_name:?}"
    )),
    Ok(false) => host
      .diagnostics
      .push(format!("documentElement.className already {class_name:?}")),
    Err(err) => host.diagnostics.push(format!(
      "failed to set documentElement.className = {class_name:?}: {err:?}"
    )),
  }
}

fn dom2_is_html_namespace(namespace: &str) -> bool {
  namespace.is_empty() || namespace == fastrender::dom::HTML_NAMESPACE
}

fn dom2_attr_name_matches(existing: &str, query: &str, is_html: bool) -> bool {
  if is_html {
    existing.eq_ignore_ascii_case(query)
  } else {
    existing == query
  }
}

fn dom2_get_attribute<'a>(dom: &'a Dom2Document, node: Dom2NodeId, name: &str) -> Option<&'a str> {
  let kind = &dom.node(node).kind;
  let (attrs, is_html) = match kind {
    Dom2NodeKind::Element {
      namespace,
      attributes,
      ..
    }
    | Dom2NodeKind::Slot {
      namespace,
      attributes,
      ..
    } => (attributes, dom2_is_html_namespace(namespace)),
    _ => return None,
  };
  attrs
    .iter()
    .find(|(k, _)| dom2_attr_name_matches(k.as_str(), name, is_html))
    .map(|(_, v)| v.as_str())
}

fn dom2_set_attribute(
  dom: &mut Dom2Document,
  node: Dom2NodeId,
  name: &str,
  value: &str,
) -> std::result::Result<bool, &'static str> {
  let kind = &mut dom.node_mut(node).kind;
  let (attrs, is_html) = match kind {
    Dom2NodeKind::Element {
      namespace,
      attributes,
      ..
    }
    | Dom2NodeKind::Slot {
      namespace,
      attributes,
      ..
    } => (attributes, dom2_is_html_namespace(namespace)),
    _ => return Err("node does not support attributes"),
  };

  if let Some((_, existing)) = attrs
    .iter_mut()
    .find(|(k, _)| dom2_attr_name_matches(k.as_str(), name, is_html))
  {
    if existing == value {
      return Ok(false);
    }
    existing.clear();
    existing.push_str(value);
    return Ok(true);
  }

  attrs.push((name.to_string(), value.to_string()));
  Ok(true)
}

fn extract_document_element_class_name(script_text: &str) -> Option<String> {
  static SINGLE_QUOTE_RE: OnceLock<Option<Regex>> = OnceLock::new();
  static DOUBLE_QUOTE_RE: OnceLock<Option<Regex>> = OnceLock::new();

  let single = SINGLE_QUOTE_RE.get_or_init(|| {
    Regex::new(r#"document\s*\.\s*documentElement\s*\.\s*className\s*=\s*'([^']*)'"#).ok()
  });
  if let Some(single) = single.as_ref() {
    if let Some(caps) = single.captures(script_text) {
      return Some(caps.get(1)?.as_str().to_string());
    }
  }

  let double = DOUBLE_QUOTE_RE.get_or_init(|| {
    Regex::new(r#"document\s*\.\s*documentElement\s*\.\s*className\s*=\s*"([^"]*)""#).ok()
  });
  if let Some(double) = double.as_ref() {
    if let Some(caps) = double.captures(script_text) {
      return Some(caps.get(1)?.as_str().to_string());
    }
  }

  None
}

fn find_document_element(dom: &Dom2Document) -> Option<Dom2NodeId> {
  for id in dom.subtree_preorder(dom.root()) {
    let node = dom.node(id);
    if let Dom2NodeKind::Element {
      tag_name,
      namespace,
      ..
    } = &node.kind
    {
      if tag_name.eq_ignore_ascii_case("html")
        && (namespace.is_empty() || namespace == fastrender::dom::HTML_NAMESPACE)
      {
        return Some(id);
      }
    }
  }
  None
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
  /// When unset, defaults to (timeout - 250ms) to allow a graceful timeout before the hard kill.
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
    let mut host = JsHost::new(dom2);

    // Collect and execute inline scripts best-effort.
    let run_limits = js.run_limits();
    let max_script_bytes = if js.max_script_bytes == 0 {
      usize::MAX
    } else {
      js.max_script_bytes
    };
    let mut event_loop = EventLoop::<JsHost>::new();
    let mut scripts_queued = 0usize;

    for script_id in host.dom.subtree_preorder(host.dom.root()) {
      let node = host.dom.node(script_id);
      let Dom2NodeKind::Element { tag_name, namespace, .. } = &node.kind else {
        continue;
      };
      if !tag_name.eq_ignore_ascii_case("script")
        || !(namespace.is_empty() || namespace == fastrender::dom::HTML_NAMESPACE)
      {
        continue;
      }
      if !host.dom.is_connected_for_scripting(script_id) {
        continue;
      }

      // External scripts are out of scope for the CLI MVP.
      if dom2_get_attribute(&host.dom, script_id, "src").is_some_and(|v| !v.is_empty()) {
        log("JavaScript: skipping external <script src=...> (not supported yet)");
        continue;
      }

      let mut inline_text = String::new();
      for &child_id in &node.children {
        if let Dom2NodeKind::Text { content } = &host.dom.node(child_id).kind {
          inline_text.push_str(content);
        }
      }

      if inline_text.as_bytes().len() > max_script_bytes {
        log(&format!(
          "JavaScript: skipping inline script ({} bytes > max {})",
          inline_text.as_bytes().len(),
          max_script_bytes
        ));
        continue;
      }

      // Queue execution as tasks so microtask checkpoints are respected by the event loop.
      let script_text = inline_text;
      if let Err(err) = event_loop.queue_task(TaskSource::Script, move |dom, _event_loop| {
        apply_js_stub(dom, &script_text);
        Ok(())
      }) {
        log(&format!("JavaScript: failed to queue script task: {err}"));
        break;
      }

      scripts_queued += 1;
    }

    if scripts_queued > 0 {
      log(&format!(
        "JavaScript: queued {scripts_queued} task(s) (max_tasks={} max_microtasks={} max_wall_ms={} max_script_bytes={})",
        run_limits.max_tasks, run_limits.max_microtasks, js.max_wall_ms, max_script_bytes
      ));
      match event_loop.run_until_idle(&mut host, run_limits) {
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

    if !host.diagnostics.is_empty() {
      for line in &host.diagnostics {
        log(&format!("JavaScript: {line}"));
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
    let dom = host.dom.to_renderer_dom();
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
