use base64::engine::general_purpose::STANDARD as BASE64_STD;
use base64::Engine as _;
use fastrender::debug::runtime::RuntimeToggles;
use fastrender::resource::{FetchedResource, ResourceFetcher};
use fastrender::style::color::Rgba;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::io::{Read, Write};
use std::sync::Arc;

#[derive(Debug, Serialize, Deserialize)]
struct IframeRenderRequest {
  /// Original iframe URL (used for crash triggers / diagnostics).
  url: String,
  /// HTML payload to render. When absent, the renderer returns an error (unless `url` triggers a crash).
  html: Option<String>,
  /// Base URL used for resolving relative URLs inside the iframe document.
  base_url: Option<String>,
  /// Viewport width in CSS pixels.
  width: u32,
  /// Viewport height in CSS pixels.
  height: u32,
  /// Device pixel ratio for media queries / output scaling.
  device_pixel_ratio: f32,
  /// Maximum nested iframe depth to render.
  max_iframe_depth: usize,
}

#[derive(Debug, Serialize, Deserialize)]
struct IframeRenderResponse {
  status: String,
  message: Option<String>,
  pixel_width: Option<u32>,
  pixel_height: Option<u32>,
  premultiplied: Option<bool>,
  pixels_base64: Option<String>,
}

/// `ResourceFetcher` that rejects all external fetches.
///
/// Out-of-process iframe rendering is a security boundary; fetching is expected to be mediated by
/// the browser process. This keeps the renderer deterministic for tests and avoids unexpected
/// network/filesystem access.
#[derive(Debug, Clone, Copy, Default)]
struct RejectAllFetcher;

impl ResourceFetcher for RejectAllFetcher {
  fn fetch(&self, url: &str) -> fastrender::Result<FetchedResource> {
    Err(fastrender::Error::Other(format!(
      "RejectAllFetcher: fetch not permitted in iframe renderer (url={url:?})"
    )))
  }
}

fn is_crash_url(url: &str) -> bool {
  // `url::Url` rejects `crash://` without a host, so just do a cheap prefix check here.
  url
    .as_bytes()
    .get(..8)
    .is_some_and(|prefix| prefix.eq_ignore_ascii_case(b"crash://"))
}

fn main() {
  // Defense-in-depth: close any accidentally inherited file descriptors before doing any work.
  //
  // This renderer is intended to be spawned as a security boundary. In production, the parent
  // process should also enforce `FD_CLOEXEC`/handle allowlisting at spawn time, but closing fds in
  // the child provides an extra layer of protection against leaks.
  //
  // On macOS we avoid `pre_exec`-based fd sanitization for safety (posix_spawn), so this
  // post-exec close pass is most useful on Linux.
  #[cfg(target_os = "linux")]
  {
    if let Ok(cfg) = fastrender::system::renderer_sandbox::RendererSandboxConfig::from_env() {
      if cfg.close_fds {
        let _ = fastrender::sandbox::close_fds_except(&[0, 1, 2]);
      }
    }
  }

  let mut stdin = String::new();
  if std::io::stdin().read_to_string(&mut stdin).is_err() {
    std::process::exit(2);
  }

  let req: IframeRenderRequest = match serde_json::from_str(&stdin) {
    Ok(req) => req,
    Err(err) => {
      let resp = IframeRenderResponse {
        status: "error".to_string(),
        message: Some(format!("invalid request JSON: {err}")),
        pixel_width: None,
        pixel_height: None,
        premultiplied: None,
        pixels_base64: None,
      };
      let _ = serde_json::to_writer(std::io::stdout(), &resp);
      return;
    }
  };

  // Deterministic crash trigger for integration tests.
  if is_crash_url(&req.url) {
    std::process::abort();
  }

  let Some(html) = req.html.as_deref() else {
    let resp = IframeRenderResponse {
      status: "error".to_string(),
      message: Some("missing html payload".to_string()),
      pixel_width: None,
      pixel_height: None,
      premultiplied: None,
      pixels_base64: None,
    };
    let _ = serde_json::to_writer(std::io::stdout(), &resp);
    return;
  };

  // Avoid inheriting arbitrary host FASTR_* settings into this renderer process. The browser process
  // should pass explicit toggles/prefs via the request if/when needed.
  let mut toggles = HashMap::<String, String>::new();
  // Keep the default paint backend unless explicitly overridden.
  toggles.insert("FASTR_IFRAME_RENDERER".to_string(), "1".to_string());
  let runtime_toggles = Arc::new(RuntimeToggles::from_map(toggles));

  let mut builder = fastrender::FastRender::builder()
    .fetcher(Arc::new(RejectAllFetcher))
    .background_color(Rgba::WHITE)
    .viewport_size(req.width.max(1), req.height.max(1))
    .device_pixel_ratio(req.device_pixel_ratio)
    .max_iframe_depth(req.max_iframe_depth)
    .runtime_toggles((*runtime_toggles).clone());

  if let Some(base_url) = req.base_url.as_deref() {
    builder = builder.base_url(base_url.to_string());
  }

  let mut renderer = match builder.build() {
    Ok(renderer) => renderer,
    Err(err) => {
      let resp = IframeRenderResponse {
        status: "error".to_string(),
        message: Some(format!("failed to build renderer: {err}")),
        pixel_width: None,
        pixel_height: None,
        premultiplied: None,
        pixels_base64: None,
      };
      let _ = serde_json::to_writer(std::io::stdout(), &resp);
      return;
    }
  };

  let pixmap = match renderer.render_html(html, req.width.max(1), req.height.max(1)) {
    Ok(pixmap) => pixmap,
    Err(err) => {
      let resp = IframeRenderResponse {
        status: "error".to_string(),
        message: Some(format!("render failed: {err}")),
        pixel_width: None,
        pixel_height: None,
        premultiplied: None,
        pixels_base64: None,
      };
      let _ = serde_json::to_writer(std::io::stdout(), &resp);
      return;
    }
  };

  let resp = IframeRenderResponse {
    status: "ok".to_string(),
    message: None,
    pixel_width: Some(pixmap.width()),
    pixel_height: Some(pixmap.height()),
    premultiplied: Some(true),
    pixels_base64: Some(BASE64_STD.encode(pixmap.data())),
  };

  let mut stdout = std::io::stdout().lock();
  if serde_json::to_writer(&mut stdout, &resp).is_ok() {
    let _ = stdout.flush();
  }
}
