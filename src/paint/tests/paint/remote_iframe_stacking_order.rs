use crate::api::ResourceContext;
use crate::debug::runtime::{with_thread_runtime_toggles, RuntimeToggles};
use crate::error::Result;
use crate::geometry::Point;
use crate::image_loader::ImageCache;
use crate::paint::display_list_renderer::PaintParallelism;
use crate::paint::painter::paint_tree_display_list_layered_with_resources_scaled_offset_depth;
use crate::resource::{origin_from_url, FetchedResource, ResourceAccessPolicy, ResourceFetcher};
use crate::{FastRender, FastRenderConfig, FontConfig, LayoutParallelism, RenderOptions};
use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use tiny_skia::{Pixmap, PixmapPaint, Transform};

struct PanicFetcher;

impl ResourceFetcher for PanicFetcher {
  fn fetch(&self, url: &str) -> Result<FetchedResource> {
    panic!("unexpected fetch during OOPIF parent paint: {url}");
  }
}

#[derive(Debug)]
struct ChildHtmlFetcher {
  child_url: String,
  html: Vec<u8>,
  calls: AtomicUsize,
}

impl ChildHtmlFetcher {
  fn new(child_url: &str, html: Vec<u8>) -> Self {
    Self {
      child_url: child_url.to_string(),
      html,
      calls: AtomicUsize::new(0),
    }
  }

  fn calls(&self) -> usize {
    self.calls.load(Ordering::Relaxed)
  }
}

impl ResourceFetcher for ChildHtmlFetcher {
  fn fetch(&self, url: &str) -> Result<FetchedResource> {
    if url != self.child_url {
      panic!("unexpected fetch url: {url}");
    }
    self.calls.fetch_add(1, Ordering::Relaxed);
    Ok(FetchedResource::new(
      self.html.clone(),
      Some("text/html; charset=utf-8".to_string()),
    ))
  }
}

fn pixel_rgba(pixmap: &Pixmap, x: u32, y: u32) -> (u8, u8, u8, u8) {
  let p = pixmap.pixel(x, y).expect("pixel should exist");
  (p.red(), p.green(), p.blue(), p.alpha())
}

#[test]
fn remote_iframe_layers_preserve_parent_paint_order_for_overlays() {
  crate::testing::init_rayon_for_tests(2);

  let toggles = RuntimeToggles::from_map(HashMap::from([
    (
      "FASTR_PAINT_BACKEND".to_string(),
      "display_list".to_string(),
    ),
    ("FASTR_SITE_ISOLATION".to_string(), "1".to_string()),
  ]));
  let toggles = Arc::new(toggles);

  let config = FastRenderConfig::new()
    .with_runtime_toggles((*toggles).clone())
    .with_font_sources(FontConfig::bundled_only())
    .with_layout_parallelism(LayoutParallelism::disabled())
    .with_paint_parallelism(PaintParallelism::disabled());
  let mut renderer = FastRender::with_config(config).expect("renderer");

  let parent_url = "https://parent.test/root.html";
  let child_url = "https://child.test/child.html";

  let root_html = format!(
    r#"<!doctype html>
    <style>
      html, body {{ margin: 0; padding: 0; background: rgb(0, 0, 255); }}
      iframe {{ position: absolute; left: 10px; top: 10px; width: 40px; height: 40px; border: 0; }}
      #overlay {{ position: absolute; left: 20px; top: 20px; width: 20px; height: 20px; background: rgb(255, 0, 0); z-index: 10; }}
    </style>
    <iframe src="{child_url}"></iframe>
    <div id="overlay"></div>
    "#
  );

  let options = RenderOptions {
    viewport: Some((80, 80)),
    ..Default::default()
  };
  let prepared = renderer
    .prepare_html_with_stylesheets(&root_html, parent_url, options)
    .expect("prepare root");
  let fragment_tree = prepared.document.fragment_tree().clone();

  // Build an image cache with a fetcher that panics. If the parent accidentally tries to
  // render/fetch the cross-origin iframe in-process, the test will fail.
  let mut image_cache = ImageCache::with_fetcher(Arc::new(PanicFetcher));
  image_cache.set_base_url(parent_url);
  let origin = origin_from_url(parent_url).expect("origin");
  image_cache.set_resource_context(Some(ResourceContext {
    document_url: Some(parent_url.to_string()),
    policy: ResourceAccessPolicy {
      document_origin: Some(origin),
      ..Default::default()
    },
    ..Default::default()
  }));

  let layered = with_thread_runtime_toggles(Arc::clone(&toggles), || {
    paint_tree_display_list_layered_with_resources_scaled_offset_depth(
      &fragment_tree,
      80,
      80,
      renderer.font_context().clone(),
      image_cache,
      1.0,
      Point::ZERO,
      PaintParallelism::disabled(),
      &crate::scroll::ScrollState::default(),
      crate::api::DEFAULT_MAX_IFRAME_DEPTH,
    )
  })
  .expect("layered parent paint");

  assert_eq!(layered.slots.len(), 1, "expected one remote iframe slot");
  assert_eq!(
    layered.layers.len(),
    2,
    "expected two layers (before + after the iframe slot)"
  );

  let slot = &layered.slots[0];
  let slot_x = slot.rect.x().round() as i32;
  let slot_y = slot.rect.y().round() as i32;

  // Render the child frame in isolation.
  let child_html = r#"<!doctype html>
    <style>
      html, body { margin: 0; padding: 0; background: rgb(0, 255, 0); }
    </style>
  "#;
  let child_pixmap = renderer
    .render_html_with_stylesheets(
      child_html,
      child_url,
      RenderOptions {
        viewport: Some((40, 40)),
        ..Default::default()
      },
    )
    .expect("render child")
    .pixmap;

  // Composite: layer0 -> child -> layer1.
  let mut composed = Pixmap::new(80, 80).expect("composite pixmap");
  composed
    .data_mut()
    .copy_from_slice(layered.layers[0].data());

  let paint = PixmapPaint::default();
  composed.draw_pixmap(
    slot_x,
    slot_y,
    child_pixmap.as_ref(),
    &paint,
    Transform::identity(),
    None,
  );
  composed.draw_pixmap(
    0,
    0,
    layered.layers[1].as_ref(),
    &paint,
    Transform::identity(),
    None,
  );

  // Pixel inside both the iframe and the overlay.
  assert_eq!(pixel_rgba(&composed, 25, 25), (255, 0, 0, 255));
}

#[test]
fn sandboxed_same_origin_iframe_is_treated_as_remote_under_site_isolation() {
  crate::testing::init_rayon_for_tests(2);

  let toggles = RuntimeToggles::from_map(HashMap::from([
    (
      "FASTR_PAINT_BACKEND".to_string(),
      "display_list".to_string(),
    ),
    ("FASTR_SITE_ISOLATION".to_string(), "1".to_string()),
  ]));
  let toggles = Arc::new(toggles);

  let config = FastRenderConfig::new()
    .with_runtime_toggles((*toggles).clone())
    .with_font_sources(FontConfig::bundled_only())
    .with_layout_parallelism(LayoutParallelism::disabled())
    .with_paint_parallelism(PaintParallelism::disabled());
  let mut renderer = FastRender::with_config(config).expect("renderer");

  let parent_url = "https://parent.test/root.html";
  let child_url = "https://parent.test/child.html";

  let root_html = format!(
    r#"<!doctype html>
    <style>
      html, body {{ margin: 0; padding: 0; background: rgb(0, 0, 255); }}
      iframe {{ position: absolute; left: 10px; top: 10px; width: 40px; height: 40px; border: 0; }}
    </style>
    <iframe sandbox src="{child_url}"></iframe>
    "#
  );

  let options = RenderOptions {
    viewport: Some((80, 80)),
    ..Default::default()
  };
  let prepared = renderer
    .prepare_html_with_stylesheets(&root_html, parent_url, options)
    .expect("prepare root");
  let fragment_tree = prepared.document.fragment_tree().clone();

  // The iframe URL is same-origin, so without sandbox opaque-origin semantics the parent would try
  // to fetch/render it in-process. Use a fetcher that panics to ensure site isolation emits a remote
  // iframe slot instead.
  let mut image_cache = ImageCache::with_fetcher(Arc::new(PanicFetcher));
  image_cache.set_base_url(parent_url);
  let origin = origin_from_url(parent_url).expect("origin");
  image_cache.set_resource_context(Some(ResourceContext {
    document_url: Some(parent_url.to_string()),
    policy: ResourceAccessPolicy {
      document_origin: Some(origin),
      ..Default::default()
    },
    ..Default::default()
  }));

  let layered = with_thread_runtime_toggles(Arc::clone(&toggles), || {
    paint_tree_display_list_layered_with_resources_scaled_offset_depth(
      &fragment_tree,
      80,
      80,
      renderer.font_context().clone(),
      image_cache,
      1.0,
      Point::ZERO,
      PaintParallelism::disabled(),
      &crate::scroll::ScrollState::default(),
      crate::api::DEFAULT_MAX_IFRAME_DEPTH,
    )
  })
  .expect("layered parent paint");

  assert_eq!(layered.slots.len(), 1, "expected a remote iframe slot");
  assert_eq!(
    layered.slots[0].src, child_url,
    "expected the slot src to identify the iframe navigation"
  );
}

#[test]
fn allow_same_origin_sandboxed_same_origin_iframe_stays_in_process_under_site_isolation() {
  crate::testing::init_rayon_for_tests(2);

  let toggles = RuntimeToggles::from_map(HashMap::from([
    (
      "FASTR_PAINT_BACKEND".to_string(),
      "display_list".to_string(),
    ),
    ("FASTR_SITE_ISOLATION".to_string(), "1".to_string()),
  ]));
  let toggles = Arc::new(toggles);

  let config = FastRenderConfig::new()
    .with_runtime_toggles((*toggles).clone())
    .with_font_sources(FontConfig::bundled_only())
    .with_layout_parallelism(LayoutParallelism::disabled())
    .with_paint_parallelism(PaintParallelism::disabled());
  let mut renderer = FastRender::with_config(config).expect("renderer");

  let parent_url = "https://parent.test/root.html";
  let child_url = "https://parent.test/child.html";

  let root_html = format!(
    r#"<!doctype html>
    <style>
      html, body {{ margin: 0; padding: 0; background: rgb(0, 0, 255); }}
      iframe {{ position: absolute; left: 10px; top: 10px; width: 40px; height: 40px; border: 0; }}
    </style>
    <iframe sandbox="allow-same-origin allow-scripts" src="{child_url}"></iframe>
    "#
  );

  let options = RenderOptions {
    viewport: Some((80, 80)),
    ..Default::default()
  };
  let prepared = renderer
    .prepare_html_with_stylesheets(&root_html, parent_url, options)
    .expect("prepare root");
  let fragment_tree = prepared.document.fragment_tree().clone();

  let child_html = b"<!doctype html><style>html, body { margin:0; background: rgb(0, 255, 0); }</style>".to_vec();
  let fetcher = Arc::new(ChildHtmlFetcher::new(child_url, child_html));

  let mut image_cache = ImageCache::with_fetcher(Arc::clone(&fetcher) as Arc<dyn ResourceFetcher>);
  image_cache.set_base_url(parent_url);
  let origin = origin_from_url(parent_url).expect("origin");
  image_cache.set_resource_context(Some(ResourceContext {
    document_url: Some(parent_url.to_string()),
    policy: ResourceAccessPolicy {
      document_origin: Some(origin),
      ..Default::default()
    },
    ..Default::default()
  }));

  let layered = with_thread_runtime_toggles(Arc::clone(&toggles), || {
    paint_tree_display_list_layered_with_resources_scaled_offset_depth(
      &fragment_tree,
      80,
      80,
      renderer.font_context().clone(),
      image_cache,
      1.0,
      Point::ZERO,
      PaintParallelism::disabled(),
      &crate::scroll::ScrollState::default(),
      crate::api::DEFAULT_MAX_IFRAME_DEPTH,
    )
  })
  .expect("layered parent paint");

  assert_eq!(
    layered.slots.len(),
    0,
    "expected allow-same-origin sandboxed iframe to remain in-process"
  );
  assert!(
    fetcher.calls() > 0,
    "expected in-process iframe render to fetch the child document"
  );
}
