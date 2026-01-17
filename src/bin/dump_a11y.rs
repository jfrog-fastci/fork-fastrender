use fastrender::cli_utils as common;

use clap::Parser;
use common::args::parse_viewport;
use common::render_pipeline::read_cached_document;
use fastrender::accessibility_audit::audit_accessibility_tree;
use fastrender::api::{FastRender, RenderOptions};
use fastrender::interaction::dom_geometry;
use fastrender::resource::{
  FetchRequest, FetchedResource, HttpFetcher, ResourceFetcher, DEFAULT_ACCEPT_LANGUAGE,
  DEFAULT_USER_AGENT,
};
use fastrender::style::media::MediaType;
use std::error::Error;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;
use url::Url;

/// Dump the accessibility tree for an HTML document as JSON.
#[derive(Parser, Debug)]
#[command(name = "dump_a11y", version, about)]
struct Args {
  /// HTML file or URL to inspect
  input: String,

  /// Viewport size as WxH (e.g., 1200x800)
  #[arg(long, value_parser = parse_viewport, default_value = "1200x800")]
  viewport: (u32, u32),

  /// Device pixel ratio for media queries/srcset
  #[arg(long, default_value = "1.0")]
  dpr: f32,

  /// Output compact JSON instead of pretty-printing.
  #[arg(long)]
  compact: bool,

  /// Audit the accessibility tree for missing labels; prints issues to stderr and exits non-zero if
  /// any are found.
  #[arg(long)]
  audit: bool,

  /// When enabled, include a `bounds_css` map keyed by DOM node id (renderer preorder id),
  /// containing viewport-local CSS pixel bounds for nodes present in the accessibility tree.
  ///
  /// This triggers full style/layout (prepare) so the output can be generated without a windowed
  /// browser.
  #[arg(long)]
  include_bounds: bool,

  /// Override the User-Agent header
  #[arg(long, default_value = DEFAULT_USER_AGENT)]
  user_agent: String,

  /// Override the Accept-Language header
  #[arg(long, default_value = DEFAULT_ACCEPT_LANGUAGE)]
  accept_language: String,

  /// Abort after this many seconds
  #[arg(long)]
  timeout: Option<u64>,
}

fn main() -> Result<(), Box<dyn Error>> {
  // Avoid panicking on SIGPIPE/BrokenPipe when piped through tools like `head`.
  let default_hook = std::panic::take_hook();
  std::panic::set_hook(Box::new(move |info| {
    let mut msg = info.to_string();
    if let Some(s) = info.payload().downcast_ref::<&str>() {
      msg = (*s).to_string();
    } else if let Some(s) = info.payload().downcast_ref::<String>() {
      msg = s.clone();
    }
    if msg.contains("Broken pipe") {
      // Exit silently with success for broken pipe to mirror common CLI behavior.
      std::process::exit(0);
    }
    default_hook(info);
  }));

  let args = Args::parse();

  if let Some(sec) = args.timeout {
    std::thread::spawn(move || {
      std::thread::sleep(Duration::from_secs(sec));
      eprintln!("dump_a11y: timed out after {}s", sec);
      std::process::exit(1);
    });
  }

  let mut http_fetcher = HttpFetcher::new()
    .with_user_agent(args.user_agent.clone())
    .with_accept_language(args.accept_language.clone());
  if let Some(secs) = args.timeout {
    http_fetcher = http_fetcher.with_timeout(Duration::from_secs(secs));
  }
  let fetcher: Arc<dyn ResourceFetcher> = Arc::new(http_fetcher);

  let mut renderer = FastRender::builder()
    .device_pixel_ratio(args.dpr)
    .fetcher(Arc::clone(&fetcher))
    .build()?;
  let resource = load_document_resource(&args.input, fetcher.as_ref())?;

  let options = RenderOptions::new()
    .with_viewport(args.viewport.0, args.viewport.1)
    .with_device_pixel_ratio(args.dpr)
    .with_media_type(MediaType::Screen);

  let (tree, json) = if args.include_bounds {
    let base_hint = resource.final_url.as_deref().unwrap_or("");
    if !base_hint.trim().is_empty() {
      renderer.set_base_url(base_hint.to_string());
    }

    let html = fastrender::html::encoding::decode_html_bytes(
      &resource.bytes,
      resource.content_type.as_deref(),
    );
    let prepared = renderer.prepare_html(&html, options.clone())?;

    let tree =
      fastrender::accessibility::build_accessibility_tree(prepared.styled_tree(), None)?;
    let mut json = serde_json::to_value(&tree)?;

    let scroll_state = prepared.default_scroll_state();
    let mut dom_node_ids = std::collections::BTreeSet::<usize>::new();
    collect_dom_node_ids_from_a11y_tree(&tree, &mut dom_node_ids);

    let dom_node_ids: Vec<usize> = dom_node_ids.into_iter().collect();
    let bounds = dom_geometry::viewport_bounds_for_dom_node_ids(
      &prepared,
      &scroll_state,
      dom_node_ids.as_slice(),
    );

    let mut bounds_css = serde_json::Map::new();
    for dom_node_id in dom_node_ids {
      let Some(rect) = bounds.get(&dom_node_id).copied() else {
        continue;
      };
      let sanitize = |v: f32| if v.is_finite() { v } else { 0.0 };
      bounds_css.insert(
        dom_node_id.to_string(),
        serde_json::json!({
          "x": sanitize(rect.x()),
          "y": sanitize(rect.y()),
          "width": sanitize(rect.width()).max(0.0),
          "height": sanitize(rect.height()).max(0.0),
        }),
      );
    }

    match json.as_object_mut() {
      Some(obj) => {
        obj.insert(
          "bounds_css".to_string(),
          serde_json::Value::Object(bounds_css),
        );
      }
      None => {
        json = serde_json::json!({
          "tree": json,
          "bounds_css": bounds_css,
        });
      }
    }

    (tree, json)
  } else {
    let tree = renderer.accessibility_tree_fetched_html(&resource, None, options)?;
    let json = serde_json::to_value(&tree)?;
    (tree, json)
  };

  let audit_issues = args.audit.then(|| audit_accessibility_tree(&tree));

  if args.compact {
    println!("{}", serde_json::to_string(&json)?);
  } else {
    println!("{}", serde_json::to_string_pretty(&json)?);
  }

  if let Some(issues) = audit_issues {
    if !issues.is_empty() {
      for issue in &issues {
        eprintln!(
          "a11y-audit: node {} role={} {}",
          issue.node_id, issue.role, issue.message
        );
      }
      return Err(Box::new(std::io::Error::new(
        std::io::ErrorKind::Other,
        format!("accessibility audit found {} issue(s)", issues.len()),
      )));
    }
  }

  Ok(())
}

fn collect_dom_node_ids_from_a11y_tree(
  root: &fastrender::accessibility::AccessibilityNode,
  out: &mut std::collections::BTreeSet<usize>,
) {
  let mut stack: Vec<&fastrender::accessibility::AccessibilityNode> = vec![root];
  while let Some(node) = stack.pop() {
    out.insert(node.node_id);
    for child in node.children.iter().rev() {
      stack.push(child);
    }
  }
}

fn load_document_resource(
  input: &str,
  fetcher: &dyn ResourceFetcher,
) -> Result<FetchedResource, Box<dyn Error>> {
  if let Ok(url) = Url::parse(input) {
    if url.scheme() == "file" {
      let path_buf = url.to_file_path().map_err(|_| {
        std::io::Error::new(std::io::ErrorKind::InvalidInput, "invalid file:// path")
      })?;
      let cached = read_cached_document(&path_buf)?;
      return Ok(cached.resource);
    }

    let mut resource = fetcher.fetch_with_request(FetchRequest::document(url.as_str()))?;
    resource
      .final_url
      .get_or_insert_with(|| url.as_str().to_string());
    return Ok(resource);
  }

  let path = Path::new(input);
  let cached = read_cached_document(path)?;
  Ok(cached.resource)
}
