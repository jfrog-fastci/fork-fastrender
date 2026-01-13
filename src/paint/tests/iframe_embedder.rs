use crate::debug::runtime::RuntimeToggles;
use crate::paint::iframe::{
  iframe_is_cross_origin, DefaultIframeEmbedder, IframeEmbedder, IframePaintAction, IframePaintInfo,
};
use crate::resource::{FetchedResource, ResourceFetcher};
use crate::{FastRender, FastRenderConfig, RenderOptions};
use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use tiny_skia::Pixmap;

fn rgba(pixmap: &Pixmap, x: u32, y: u32) -> (u8, u8, u8, u8) {
  let px = pixmap.pixel(x, y).unwrap();
  (px.red(), px.green(), px.blue(), px.alpha())
}

#[derive(Clone, Default)]
struct MapFetcher {
  calls: Arc<AtomicUsize>,
  map: HashMap<String, FetchedResource>,
}

impl MapFetcher {
  fn with_html(mut self, url: &str, html: &str) -> Self {
    let mut res = FetchedResource::with_final_url(
      html.as_bytes().to_vec(),
      Some("text/html; charset=utf-8".to_string()),
      Some(url.to_string()),
    );
    res.status = Some(200);
    self.map.insert(url.to_string(), res);
    self
  }

  fn calls(&self) -> usize {
    self.calls.load(Ordering::SeqCst)
  }
}

impl ResourceFetcher for MapFetcher {
  fn fetch(&self, url: &str) -> crate::Result<FetchedResource> {
    self.calls.fetch_add(1, Ordering::SeqCst);
    Ok(
      self
        .map
        .get(url)
        .unwrap_or_else(|| panic!("unexpected fetch: {url}"))
        .clone(),
    )
  }
}

#[derive(Debug, Clone)]
struct RecordedIframeCall {
  info: IframePaintInfo,
  srcdoc_html: Option<String>,
}

#[derive(Default)]
struct RecordingEmbedder {
  calls: Mutex<Vec<RecordedIframeCall>>,
}

impl RecordingEmbedder {
  fn calls(&self) -> Vec<RecordedIframeCall> {
    self
      .calls
      .lock()
      .unwrap_or_else(|poisoned| poisoned.into_inner())
      .clone()
  }
}

impl IframeEmbedder for RecordingEmbedder {
  fn iframe_paint_action(
    &self,
    info: &IframePaintInfo,
    srcdoc_html: Option<&str>,
    style: Option<&crate::style::ComputedStyle>,
    image_cache: &crate::image_loader::ImageCache,
    font_ctx: &crate::text::font_loader::FontContext,
    device_pixel_ratio: f32,
    max_iframe_depth: usize,
    referrer_policy: Option<crate::resource::ReferrerPolicy>,
  ) -> IframePaintAction {
    self
      .calls
      .lock()
      .unwrap_or_else(|poisoned| poisoned.into_inner())
      .push(RecordedIframeCall {
        info: info.clone(),
        srcdoc_html: srcdoc_html.map(|s| s.to_string()),
      });

    // MVP isolation policy:
    // - keep `srcdoc` browsing contexts inline (they inherit origin),
    // - keep about:blank inline (same-origin),
    // - stop in-process rendering for cross-origin `src` iframes.
    if srcdoc_html.is_none() {
      let ctx = image_cache.resource_context();
      let document_origin = ctx
        .as_ref()
        .and_then(|ctx| ctx.policy.document_origin.as_ref());
      if iframe_is_cross_origin(document_origin, info.url.as_str()) {
        return IframePaintAction::RemotePlaceholder;
      }
    }

    DefaultIframeEmbedder.iframe_paint_action(
      info,
      srcdoc_html,
      style,
      image_cache,
      font_ctx,
      device_pixel_ratio,
      max_iframe_depth,
      referrer_policy,
    )
  }
}

fn make_renderer(backend: &str, fetcher: Arc<dyn ResourceFetcher>) -> FastRender {
  let toggles = RuntimeToggles::from_map(HashMap::from([(
    "FASTR_PAINT_BACKEND".to_string(),
    backend.to_string(),
  )]));
  let config = FastRenderConfig::new()
    .with_runtime_toggles(toggles)
    .with_base_url("https://example.test/");
  FastRender::with_config_and_fetcher(config, Some(fetcher)).expect("create renderer")
}

fn assert_single_call(embedder: &RecordingEmbedder, expected_url: &str, expect_srcdoc: bool) {
  let calls = embedder.calls();
  assert_eq!(
    calls.len(),
    1,
    "expected a single iframe callback, got {calls:?}"
  );
  let call = &calls[0];
  assert_eq!(call.info.url, expected_url);
  assert_eq!(call.info.is_srcdoc, expect_srcdoc);
  assert!(
    call.info.stable_id > 0,
    "expected non-zero stable_id for iframe, got {}",
    call.info.stable_id
  );
}

#[test]
fn iframe_embedder_cross_origin_uses_remote_placeholder_and_skips_fetch() {
  for backend in ["legacy", "display_list"] {
    let fetcher = Arc::new(MapFetcher::default());
    let embedder = Arc::new(RecordingEmbedder::default());
    let mut renderer = make_renderer(backend, fetcher.clone());

    let outer_bg = (10, 20, 30, 255);
    let iframe_url = "https://other.test/iframe.html";
    let html = format!(
      "<!doctype html>\
       <style>\
         html,body{{margin:0;background:rgb({},{},{});}}\
         iframe{{display:block;width:20px;height:20px;border:0;background:transparent;}}\
       </style>\
       <iframe src=\"{iframe_url}\"></iframe>",
      outer_bg.0, outer_bg.1, outer_bg.2
    );

    let options = RenderOptions::new()
      .with_viewport(64, 64)
      .with_iframe_embedder(Some(embedder.clone()));
    let pixmap = renderer
      .render_html_with_options(&html, options)
      .expect("render");

    assert_eq!(
      rgba(&pixmap, 10, 10),
      outer_bg,
      "expected iframe area to remain unpainted for backend={backend}"
    );
    assert_eq!(
      fetcher.calls(),
      0,
      "expected no subresource fetches for backend={backend}"
    );
    assert_single_call(embedder.as_ref(), iframe_url, false);
  }
}

#[test]
fn iframe_embedder_allows_same_origin_iframe_inline() {
  for backend in ["legacy", "display_list"] {
    let iframe_url = "https://example.test/iframe.html";
    let inner_bg = (200, 40, 10, 255);
    let inner_html = format!(
      "<!doctype html><style>html,body{{margin:0;background:rgb({},{},{});}}</style>",
      inner_bg.0, inner_bg.1, inner_bg.2
    );

    let fetcher = Arc::new(MapFetcher::default().with_html(iframe_url, &inner_html));
    let embedder = Arc::new(RecordingEmbedder::default());
    let mut renderer = make_renderer(backend, fetcher.clone());

    let html = format!(
      "<!doctype html>\
       <style>\
         html,body{{margin:0;background:rgb(10,20,30);}}\
         iframe{{display:block;width:20px;height:20px;border:0;background:transparent;}}\
       </style>\
       <iframe src=\"{iframe_url}\"></iframe>",
    );

    let options = RenderOptions::new()
      .with_viewport(64, 64)
      .with_iframe_embedder(Some(embedder.clone()));
    let pixmap = renderer
      .render_html_with_options(&html, options)
      .expect("render");

    assert_eq!(
      rgba(&pixmap, 10, 10),
      inner_bg,
      "expected iframe contents to paint inline for backend={backend}"
    );
    assert_eq!(
      fetcher.calls(),
      1,
      "expected iframe document fetch for backend={backend}"
    );
    assert_single_call(embedder.as_ref(), iframe_url, false);
  }
}

#[test]
fn iframe_embedder_allows_srcdoc_iframe_inline_without_fetch() {
  for backend in ["legacy", "display_list"] {
    let fetcher = Arc::new(MapFetcher::default());
    let embedder = Arc::new(RecordingEmbedder::default());
    let mut renderer = make_renderer(backend, fetcher.clone());

    let inner_bg = (0, 255, 0, 255);
    let inner_html = format!(
      "<!doctype html><style>html,body{{margin:0;background:rgb({},{},{});}}</style>",
      inner_bg.0, inner_bg.1, inner_bg.2
    );
    let html = format!(
      "<!doctype html>\
       <style>\
         html,body{{margin:0;background:rgb(10,20,30);}}\
         iframe{{display:block;width:20px;height:20px;border:0;background:transparent;}}\
       </style>\
       <iframe srcdoc='{inner_html}'></iframe>",
    );

    let options = RenderOptions::new()
      .with_viewport(64, 64)
      .with_iframe_embedder(Some(embedder.clone()));
    let pixmap = renderer
      .render_html_with_options(&html, options)
      .expect("render");

    assert_eq!(
      rgba(&pixmap, 10, 10),
      inner_bg,
      "expected srcdoc iframe contents to paint inline for backend={backend}"
    );
    assert_eq!(
      fetcher.calls(),
      0,
      "expected no network fetches for srcdoc iframe for backend={backend}"
    );
    // `src` defaults to about:blank when absent.
    assert_single_call(embedder.as_ref(), "about:blank", true);
  }
}
