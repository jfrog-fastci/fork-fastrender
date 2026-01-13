//! Render the browser chrome frame HTML/CSS into a PNG for rapid iteration.
//!
//! This is intended as a dev/debug aid for the "renderer_chrome" workstream: it lets us iterate on
//! chrome HTML/CSS without running the full windowed browser UI.
//!
//! Example:
//!   timeout -k 10 600 bash scripts/run_limited.sh --as 64G -- \
//!     bash scripts/cargo_agent.sh run --bin render_chrome_frame -- \
//!       out.png --width 1200 --height 800 --dpr 2

#![allow(clippy::io_other_error)]
#![allow(clippy::items_after_test_module)]
#![allow(clippy::len_zero)]
#![allow(clippy::redundant_closure)]

use clap::Parser;
use fastrender::api::{FastRender, FastRenderConfig, RenderOptions};
use fastrender::debug::inspect::{inspect, InspectQuery, RectSnapshot};
use fastrender::error::ResourceError;
use fastrender::image_output::encode_image;
use fastrender::resource::{FetchedResource, HttpFetcher, ResourceFetcher};
use fastrender::{Error, OutputFormat, Result};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use url::Url;

/// Render the built-in chrome frame HTML/CSS to a PNG.
#[derive(Parser, Debug)]
#[command(name = "render_chrome_frame", version, about)]
struct Args {
  /// Output PNG path.
  output: PathBuf,

  /// Viewport width in CSS pixels.
  #[arg(long, default_value_t = 1200)]
  width: u32,

  /// Viewport height in CSS pixels.
  #[arg(long, default_value_t = 800)]
  height: u32,

  /// Device pixel ratio.
  #[arg(long, default_value_t = 1.0)]
  dpr: f32,
}

struct ChromeAssetFetcher {
  chrome_root: PathBuf,
  manifest_dir: PathBuf,
  fallback: HttpFetcher,
}

impl ChromeAssetFetcher {
  fn new() -> Self {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let chrome_root = manifest_dir.join("assets/chrome_frame");
    Self {
      chrome_root,
      manifest_dir,
      fallback: HttpFetcher::new(),
    }
  }

  fn guess_content_type(path: &Path) -> Option<String> {
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    Some(
      match ext.as_str() {
        "html" | "htm" => "text/html",
        "css" => "text/css",
        "js" => "application/javascript",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "woff" => "font/woff",
        "woff2" => "font/woff2",
        "ttf" => "font/ttf",
        "otf" => "font/otf",
        _ => return None,
      }
      .to_string(),
    )
  }

  fn sanitize_rel_path(url: &str, rel: &str) -> Result<PathBuf> {
    let mut out = PathBuf::new();
    for component in Path::new(rel).components() {
      match component {
        Component::Normal(part) => out.push(part),
        Component::CurDir => {}
        Component::RootDir | Component::ParentDir | Component::Prefix(_) => {
          return Err(Error::Resource(ResourceError::new(
            url,
            format!("invalid chrome asset path: {rel:?}"),
          )));
        }
      }
    }
    Ok(out)
  }

  fn read_local(&self, url: &str, path: &Path) -> Result<FetchedResource> {
    let bytes = std::fs::read(path).map_err(|err| {
      Error::Resource(ResourceError::new(url, format!("failed to read {path:?}")).with_source(err))
    })?;
    let mut res = FetchedResource::new(bytes, Self::guess_content_type(path));
    res.status = Some(200);
    res.final_url = Some(url.to_string());
    Ok(res)
  }

  fn fetch_chrome_url(&self, url: &str) -> Result<FetchedResource> {
    let parsed = Url::parse(url).map_err(|err| {
      Error::Resource(ResourceError::new(url, format!("invalid URL: {err}")).with_source(err))
    })?;

    let host = parsed.host_str().unwrap_or_default();
    if host != "chrome-frame" {
      return Err(Error::Resource(ResourceError::new(
        url,
        format!("unknown chrome host {host:?} (expected \"chrome-frame\")"),
      )));
    }

    let path = match parsed.path() {
      "" | "/" => "index.html",
      raw => raw.strip_prefix('/').unwrap_or(raw),
    };
    let rel = Self::sanitize_rel_path(url, path)?;

    // Special-case `chrome://chrome-frame/icons/...` to reuse the existing SVG icon set.
    let resolved = if let Ok(stripped) = rel.strip_prefix("icons") {
      // `strip_prefix` doesn't consume the separator; `icons/` becomes `icons` + `<rest>`.
      // Ensure the remaining path is well-formed.
      let mut rest = PathBuf::new();
      for component in stripped.components() {
        match component {
          Component::Normal(part) => rest.push(part),
          Component::CurDir => {}
          Component::RootDir | Component::ParentDir | Component::Prefix(_) => {
            return Err(Error::Resource(ResourceError::new(
              url,
              "invalid icon path".to_string(),
            )));
          }
        }
      }
      self.manifest_dir.join("assets/browser_icons").join(rest)
    } else {
      self.chrome_root.join(rel)
    };

    self.read_local(url, &resolved)
  }
}

impl ResourceFetcher for ChromeAssetFetcher {
  fn fetch(&self, url: &str) -> Result<FetchedResource> {
    let parsed = Url::parse(url).map_err(|err| {
      Error::Resource(ResourceError::new(url, format!("invalid URL: {err}")).with_source(err))
    })?;

    match parsed.scheme() {
      "chrome" => self.fetch_chrome_url(url),
      // Allow local-only non-network schemes for convenience (e.g. `data:` images).
      "data" | "file" => self.fallback.fetch(url),
      // Never allow network access from this dev tool.
      "http" | "https" => Err(Error::Resource(ResourceError::new(
        url,
        "network fetches are disabled for render_chrome_frame".to_string(),
      ))),
      scheme => Err(Error::Resource(ResourceError::new(
        url,
        format!("unsupported URL scheme for render_chrome_frame: {scheme}"),
      ))),
    }
  }
}

fn union_rects(rects: impl IntoIterator<Item = RectSnapshot>) -> Option<RectSnapshot> {
  let mut min_x = f32::INFINITY;
  let mut min_y = f32::INFINITY;
  let mut max_x = f32::NEG_INFINITY;
  let mut max_y = f32::NEG_INFINITY;

  for rect in rects {
    if !(rect.x.is_finite()
      && rect.y.is_finite()
      && rect.width.is_finite()
      && rect.height.is_finite())
    {
      continue;
    }
    min_x = min_x.min(rect.x);
    min_y = min_y.min(rect.y);
    max_x = max_x.max(rect.x + rect.width);
    max_y = max_y.max(rect.y + rect.height);
  }

  if !min_x.is_finite() || !min_y.is_finite() || !max_x.is_finite() || !max_y.is_finite() {
    return None;
  }

  Some(RectSnapshot {
    x: min_x,
    y: min_y,
    width: (max_x - min_x).max(0.0),
    height: (max_y - min_y).max(0.0),
  })
}

fn run() -> Result<()> {
  let args = Args::parse();

  if args.width == 0 || args.height == 0 {
    return Err(Error::Other(format!(
      "Invalid viewport: width={}, height={}",
      args.width, args.height
    )));
  }
  if !args.dpr.is_finite() || args.dpr <= 0.0 {
    return Err(Error::Other(format!("Invalid dpr: {}", args.dpr)));
  }

  let fetcher: Arc<dyn ResourceFetcher> = Arc::new(ChromeAssetFetcher::new());

  let config = FastRenderConfig::new()
    .with_default_viewport(args.width, args.height)
    .with_device_pixel_ratio(args.dpr);
  let mut renderer = FastRender::with_config_and_fetcher(config, Some(Arc::clone(&fetcher)))?;

  let html_res = fetcher.fetch("chrome://chrome-frame/index.html")?;
  let html = String::from_utf8_lossy(&html_res.bytes).into_owned();

  let options = RenderOptions::new()
    .with_viewport(args.width, args.height)
    .with_device_pixel_ratio(args.dpr);

  let report =
    renderer.prepare_html_with_stylesheets(&html, "chrome://chrome-frame/index.html", options)?;

  // Best-effort: print the content frame rect if present.
  match inspect(
    report.document.dom(),
    report.document.styled_tree(),
    &report.document.box_tree().root,
    report.document.fragment_tree(),
    InspectQuery::Id("content-frame".to_string()),
  ) {
    Ok(matches) => {
      if let Some(first) = matches.first() {
        if let Some(rect) = union_rects(first.fragments.iter().map(|f| f.bounds.clone())) {
          println!(
            "#content-frame rect: x={:.2} y={:.2} w={:.2} h={:.2}",
            rect.x, rect.y, rect.width, rect.height
          );
        }
      }
    }
    Err(err) => {
      eprintln!("warning: failed to inspect #content-frame: {err}");
    }
  }

  let pixmap = report.document.paint_default()?;
  let png = encode_image(&pixmap, OutputFormat::Png)?;

  if let Some(parent) = args.output.parent() {
    if !parent.as_os_str().is_empty() {
      std::fs::create_dir_all(parent).map_err(Error::Io)?;
    }
  }
  std::fs::write(&args.output, png).map_err(Error::Io)?;
  eprintln!("wrote {}", args.output.display());
  Ok(())
}

fn main() {
  if let Err(err) = run() {
    eprintln!("render_chrome_frame failed: {err}");
    std::process::exit(1);
  }
}
