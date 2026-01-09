use fastrender::{
  BrowserDocument, BrowserDocument2, Error, FastRender, FastRenderConfig, RenderOptions, Result,
  Rgba,
};
use fastrender::debug::runtime::RuntimeToggles;
use fastrender::error::{RenderError, RenderStage};
use fastrender::interaction::dom_index::DomIndex;
use fastrender::interaction::dom_mutation;
use fastrender::resource::{FetchDestination, FetchRequest, FetchedResource, ResourceFetcher};
use fastrender::style::cascade::StyledNode;
use std::collections::HashMap;
use std::io;
use std::sync::{Arc, Mutex};

#[derive(Debug, Clone, PartialEq, Eq)]
struct RecordedRequest {
  url: String,
  destination: FetchDestination,
  referrer_url: Option<String>,
}

#[derive(Default)]
struct RecordingRequestFetcher {
  responses: HashMap<String, (Vec<u8>, Option<String>)>,
  requests: Arc<Mutex<Vec<RecordedRequest>>>,
}

impl RecordingRequestFetcher {
  fn with_entry(mut self, url: &str, body: &str, content_type: &str) -> Self {
    self.responses.insert(
      url.to_string(),
      (body.as_bytes().to_vec(), Some(content_type.to_string())),
    );
    self
  }

  fn requests(&self) -> Vec<RecordedRequest> {
    self
      .requests
      .lock()
      .unwrap_or_else(|poisoned| poisoned.into_inner())
      .clone()
  }
}

impl ResourceFetcher for RecordingRequestFetcher {
  fn fetch(&self, url: &str) -> Result<FetchedResource> {
    let Some((bytes, content_type)) = self.responses.get(url) else {
      return Err(Error::Io(io::Error::new(
        io::ErrorKind::NotFound,
        format!("missing resource: {url}"),
      )));
    };
    Ok(FetchedResource::new(bytes.clone(), content_type.clone()))
  }

  fn fetch_with_request(&self, req: FetchRequest<'_>) -> Result<FetchedResource> {
    self
      .requests
      .lock()
      .unwrap_or_else(|poisoned| poisoned.into_inner())
      .push(RecordedRequest {
        url: req.url.to_string(),
        destination: req.destination,
        referrer_url: req.referrer_url.map(|r| r.to_string()),
      });
    self.fetch(req.url)
  }
}

fn find_by_id<'a>(node: &'a StyledNode, id: &str) -> Option<&'a StyledNode> {
  if node
    .node
    .get_attribute_ref("id")
    .is_some_and(|value| value.eq_ignore_ascii_case(id))
  {
    return Some(node);
  }
  for child in node.children.iter() {
    if let Some(found) = find_by_id(child, id) {
      return Some(found);
    }
  }
  None
}

#[test]
fn browser_document_rerenders_after_dom_mutation() -> Result<()> {
  let options = RenderOptions::new().with_viewport(64, 64);
  let html_a = r#"
    <html>
      <head>
        <style>
          html, body { margin: 0; padding: 0; }
          #box { width: 64px; height: 64px; }
          .a { background: rgb(255, 0, 0); }
          .b { background: rgb(0, 0, 255); }
        </style>
      </head>
      <body>
        <div id="box" class="a"></div>
      </body>
    </html>
  "#;
  let html_b = r#"
    <html>
      <head>
        <style>
          html, body { margin: 0; padding: 0; }
          #box { width: 64px; height: 64px; }
          .a { background: rgb(255, 0, 0); }
          .b { background: rgb(0, 0, 255); }
        </style>
      </head>
      <body>
        <div id="box" class="b"></div>
      </body>
    </html>
  "#;

  let mut renderer = FastRender::new()?;
  let baseline_a = renderer.render_html_with_options(html_a, options.clone())?;
  let baseline_b = renderer.render_html_with_options(html_b, options.clone())?;

  let mut doc = BrowserDocument::from_html(html_a, options)?;
  let frame1 = doc.render_frame()?;
  assert_eq!(
    frame1.data(),
    baseline_a.data(),
    "first BrowserDocument frame should match render_html_with_options output"
  );

  let changed = doc.mutate_dom(|dom| {
    let mut index = DomIndex::build(dom);
    let node_id = *index
      .id_by_element_id
      .get("box")
      .expect("expected #box element");
    index
      .with_node_mut(node_id, |node| dom_mutation::set_attr(node, "class", "b"))
      .unwrap_or(false)
  });
  assert!(changed, "expected class mutation to report a change");

  let frame2 = doc
    .render_if_needed()?
    .expect("expected BrowserDocument to produce a new frame after mutation");
  assert_ne!(frame2.data(), frame1.data(), "expected pixmap to change");
  assert_eq!(
    frame2.data(),
    baseline_b.data(),
    "mutated BrowserDocument frame should match baseline B"
  );

  assert!(
    doc.render_if_needed()?.is_none(),
    "expected render_if_needed() to return None when nothing changed"
  );

  Ok(())
}

#[test]
fn browser_document_render_frame_with_scroll_state_syncs_scroll_state() -> Result<()> {
  let html = r#"<!doctype html>
    <html>
      <head>
        <style>
          html, body { margin: 0; padding: 0; }
          html { scroll-snap-type: y mandatory; }
          .snap { height: 100px; scroll-snap-align: start; }
        </style>
      </head>
      <body>
        <div class="snap"></div>
        <div class="snap"></div>
      </body>
    </html>
  "#;
  let options = RenderOptions::new().with_viewport(100, 100);
  let mut doc = BrowserDocument::from_html(html, options)?;
  doc.set_scroll(0.0, 60.0);

  let frame = doc.render_frame_with_scroll_state()?;
  assert!(
    (frame.scroll_state.viewport.y - 100.0).abs() < 1.0,
    "expected scroll snap to adjust viewport scroll, got {:?}",
    frame.scroll_state.viewport
  );
  assert_eq!(doc.scroll_state(), frame.scroll_state);
  Ok(())
}

#[test]
fn browser_document_document_url_is_used_for_referrer_when_base_href_overrides_resolution() -> Result<()> {
  let html = r#"<!doctype html><html><head>
    <base href="https://cdn.example/">
    <link rel="stylesheet" href="style.css">
  </head><body><div>hello</div></body></html>"#;

  let document_url = "https://page.example/index.html";
  let stylesheet_url = "https://cdn.example/style.css";

  let fetcher = Arc::new(RecordingRequestFetcher::default().with_entry(
    stylesheet_url,
    "body { background: rgb(1, 2, 3); }",
    "text/css",
  ));
  let toggles = RuntimeToggles::from_map(HashMap::from([(
    "FASTR_FETCH_LINK_CSS".to_string(),
    "1".to_string(),
  )]));
  let config = FastRenderConfig::default().with_runtime_toggles(toggles);
  let renderer =
    FastRender::with_config_and_fetcher(config, Some(fetcher.clone() as Arc<dyn ResourceFetcher>))?;

  let mut doc = BrowserDocument::new(renderer, html, RenderOptions::new().with_viewport(64, 64))?;
  doc.set_document_url(Some(document_url.to_string()));
  assert_eq!(doc.document_url(), Some(document_url));
  doc.render_frame()?;

  let requests = fetcher.requests();
  let stylesheet_request = requests
    .iter()
    .find(|request| request.destination == FetchDestination::Style)
    .expect("stylesheet request");
  assert_eq!(stylesheet_request.url, stylesheet_url);
  assert_eq!(stylesheet_request.referrer_url.as_deref(), Some(document_url));
  Ok(())
}

#[test]
fn browser_document_target_pseudo_uses_document_url_fragment_when_base_href_overrides_base_url() -> Result<()> {
  let html = r#"<!doctype html><html><head>
    <base href="https://cdn.example/">
    <style>#t:target { background-color: rgb(1,2,3); }</style>
  </head><body><div id="t"></div></body></html>"#;

  let url_with_fragment = "https://page.example/index.html#t";
  let mut doc = BrowserDocument::from_html(html, RenderOptions::new().with_viewport(64, 64))?;
  doc.set_document_url(Some(url_with_fragment.to_string()));
  doc.set_navigation_urls(
    Some(url_with_fragment.to_string()),
    Some(url_with_fragment.to_string()),
  );
  doc.render_frame()?;

  let prepared = doc.prepared().expect("prepared document");
  let target_node = find_by_id(prepared.styled_tree(), "t").expect("expected #t element");
  assert_eq!(
    target_node.styles.background_color,
    Rgba {
      r: 1,
      g: 2,
      b: 3,
      a: 1.0,
    }
  );
  Ok(())
}

#[test]
fn browser_document_cached_paint_respects_cancel_callback() -> Result<()> {
  let html = r#"<!doctype html>
    <html>
      <head>
        <style>
          html, body { margin: 0; padding: 0; }
          #box { width: 64px; height: 64px; background: rgb(255, 0, 0); }
        </style>
      </head>
      <body>
        <div id="box"></div>
      </body>
    </html>"#;

  let mut renderer = FastRender::new()?;
  let base_options = RenderOptions::new().with_viewport(64, 64);
  let prepared = renderer.prepare_html(html, base_options.clone())?;

  let cancel_callback: Arc<fastrender::CancelCallback> = Arc::new(|| true);
  let options = base_options.with_cancel_callback(Some(cancel_callback));
  let mut doc = BrowserDocument::from_prepared(renderer, prepared, options)?;

  let err = doc
    .render_if_needed()
    .expect_err("cached paint should respect cancel callback");
  match err {
    Error::Render(RenderError::Timeout { stage, .. }) => assert_eq!(stage, RenderStage::Paint),
    other => panic!("unexpected error: {other:?}"),
  }
  Ok(())
}

#[test]
fn browser_document2_cached_paint_respects_cancel_callback() -> Result<()> {
  use std::sync::atomic::{AtomicBool, Ordering};

  let html = r#"<!doctype html>
    <html>
      <head>
        <style>
          html, body { margin: 0; padding: 0; }
          #box { width: 64px; height: 64px; background: rgb(255, 0, 0); }
        </style>
      </head>
      <body>
        <div id="box"></div>
      </body>
    </html>"#;

  let cancelled = Arc::new(AtomicBool::new(false));
  let cancelled_for_cb = Arc::clone(&cancelled);
  let cancel_callback: Arc<fastrender::CancelCallback> =
    Arc::new(move || cancelled_for_cb.load(Ordering::SeqCst));

  let options = RenderOptions::new()
    .with_viewport(64, 64)
    .with_cancel_callback(Some(cancel_callback));
  let mut doc = BrowserDocument2::from_html(html, options)?;

  // First frame should complete so we can exercise cached paint on the next frame.
  doc.render_frame()?;
  cancelled.store(true, Ordering::SeqCst);

  let err = doc
    .render_frame()
    .expect_err("cached paint should respect cancel callback");
  match err {
    Error::Render(RenderError::Timeout { stage, .. }) => assert_eq!(stage, RenderStage::Paint),
    other => panic!("unexpected error: {other:?}"),
  }
  Ok(())
}
