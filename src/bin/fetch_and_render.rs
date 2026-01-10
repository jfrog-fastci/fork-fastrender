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
use fastrender::api::{BrowserTab, FastRenderPool, FastRenderPoolConfig};
use fastrender::image_output::encode_image;
use fastrender::js::JsExecutionOptions;
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
use std::error::Error;
use std::path::{Path, PathBuf};
use std::sync::mpsc::channel;
use std::sync::mpsc::RecvTimeoutError;
use std::sync::Arc;
use std::thread;
use std::time::Duration;
const DEFAULT_ASSET_CACHE_DIR: &str = "fetches/assets";
const DEFAULT_JS_MAX_TASKS: usize = 1024;
const DEFAULT_JS_MAX_MICROTASKS: usize = 4096;
const DEFAULT_JS_MAX_WALL_MS: u64 = 500;
const DEFAULT_JS_MAX_SCRIPT_BYTES: usize = 256 * 1024;

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
  /// The buffer is 5% of the hard timeout, clamped to 250ms..10s (i.e. at minimum, timeout - 250ms).
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
  js_enabled: bool,
  js_execution_options: JsExecutionOptions,
) -> Result<()> {
  let RenderConfigBundle { options, .. } = bundle;
  let mut log = |line: &str| println!("{line}");

  let render_result = if !js_enabled {
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

    render_pool.with_renderer(|renderer| {
      render_fetched_document(renderer, &resource, Some(&base_hint), &options)
    })?
  } else {
    log("JavaScript: enabled");

    // Ensure JavaScript execution is bounded by the same cooperative render timeout. We keep this
    // deadline alive through navigation + JS + render so `check_root` operations share one budget.
    let deadline = RenderDeadline::new(options.timeout, options.cancel_callback.clone());
    let _deadline_guard = DeadlineGuard::install(Some(&deadline));

    let factory = render_pool.factory();
    let renderer = factory.build_renderer()?;

    // For `file://` HTML loaded from disk caches, keep parity with the non-JS CLI path:
    // honor optional `.meta` sidecar URL hints as the document URL/base URL used for resolving
    // relative subresources (scripts, stylesheets, images, ...).
    //
    // We do this by registering the HTML body as an in-memory navigation target at the desired
    // document URL and then navigating to that URL via the normal `BrowserTab::navigate_to_url`
    // pipeline. This keeps script scheduling/ordering identical while allowing the document URL
    // hint to differ from the physical cache file path.
    let cached_file_document = url
      .starts_with("file://")
      .then(|| {
        let path = url.strip_prefix("file://").unwrap_or(url);
        read_cached_document(Path::new(path))
      })
      .transpose()?;

    let mut tab = BrowserTab::with_renderer_and_vmjs_and_js_execution_options(
      renderer,
      options.clone(),
      js_execution_options,
    )?;

    // Apply the base URL override (or `.meta` base hint) when the HTML is sourced from disk.
    // For non-file navigations, `BrowserTab::navigate_to_url` uses the actual navigation URL as the
    // document URL hint (browser-like behavior).
    let mut navigation_url = None::<String>;
    if let Some(cached) = cached_file_document {
      let desired_document_url = base_url_override
        .as_deref()
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| cached.document.base_hint.clone());
      tab.register_html_source(desired_document_url.clone(), cached.document.html);
      navigation_url = Some(desired_document_url);
    }

    let navigation_url = navigation_url.as_deref().unwrap_or(url);
    println!("Fetching HTML from: {navigation_url}");
    tab.navigate_to_url(navigation_url, options.clone())?;

    // Drive the JS event loop until stable with a bounded number of "frames" so hostile pages
    // cannot hang the CLI indefinitely even if they keep scheduling work.
    let max_frames = 50usize;
    match tab.run_until_stable(max_frames)? {
      fastrender::api::RunUntilStableOutcome::Stable { frames_rendered } => {
        log(&format!("JavaScript: stable (frames_rendered={frames_rendered})"));
      }
      fastrender::api::RunUntilStableOutcome::Stopped { reason, frames_rendered } => {
        log(&format!(
          "JavaScript: stopped before stable (reason={reason:?} frames_rendered={frames_rendered})"
        ));
      }
    }

    let pixmap = tab.render_frame()?;
    tab.write_trace()?;
    let diagnostics = tab.diagnostics_snapshot().unwrap_or_default();
    fastrender::api::RenderResult {
      pixmap,
      accessibility: None,
      diagnostics,
    }
  };
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
    mut config,
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
  if args.js_enabled {
    // When JavaScript execution is enabled, parse/render with "scripting enabled" semantics
    // (affects `<noscript>` handling and the `(scripting: ...)` media feature baseline).
    config = config.with_dom_scripting_enabled(true);
  }
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
  // Treat `--js-max-script-bytes 0` as "unbounded" (disables the script size limit).
  if js_options.max_script_bytes == 0 {
    js_options.max_script_bytes = usize::MAX;
  }

  // `fetch_and_render --js` uses the `vm-js` executor, which can evaluate module scripts. Enable
  // module script support so `<script type="module">` works in CLI fixtures.
  if args.js_enabled {
    js_options.supports_module_scripts = true;
  }

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
        args.js_enabled,
        js_options,
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
