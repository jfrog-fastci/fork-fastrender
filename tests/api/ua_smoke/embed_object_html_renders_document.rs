use fastrender::debug::runtime::RuntimeToggles;
use fastrender::{FastRender, FastRenderConfig, FetchedResource, ResourceFetcher};
use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use tiny_skia::Pixmap;

struct HtmlFetcher {
  expected_url: String,
  body: Vec<u8>,
  calls: AtomicUsize,
}

impl ResourceFetcher for HtmlFetcher {
  fn fetch(&self, url: &str) -> fastrender::Result<FetchedResource> {
    assert_eq!(url, self.expected_url);
    self.calls.fetch_add(1, Ordering::SeqCst);
    let mut resource = FetchedResource::new(self.body.clone(), Some("text/html".to_string()));
    resource.status = Some(200);
    Ok(resource)
  }
}

fn render_with_fetcher(backend: &str, url: &str, outer: &str) -> (Pixmap, usize) {
  let inner = "<!doctype html><style>html,body{margin:0;background:rgb(255,0,0);}</style>";
  let fetcher = Arc::new(HtmlFetcher {
    expected_url: url.to_string(),
    body: inner.as_bytes().to_vec(),
    calls: AtomicUsize::new(0),
  });
  let fetcher_dyn: Arc<dyn ResourceFetcher> = fetcher.clone();

  let toggles = RuntimeToggles::from_map(HashMap::from([(
    "FASTR_PAINT_BACKEND".to_string(),
    backend.to_string(),
  )]));
  let config = FastRenderConfig::new().with_runtime_toggles(toggles);

  let mut renderer =
    FastRender::with_config_and_fetcher(config, Some(fetcher_dyn)).expect("create renderer");
  let pixmap = renderer.render_html(outer, 32, 32).expect("render");
  let calls = fetcher.calls.load(Ordering::SeqCst);
  (pixmap, calls)
}

fn assert_embedded_document_renders(pixmap: &Pixmap) {
  let inside = pixmap.pixel(8, 8).expect("inside pixel");
  assert!(
    inside.red() > 200 && inside.green() < 80 && inside.blue() < 80,
    "expected embedded document to render red, got rgba=({}, {}, {}, {})",
    inside.red(),
    inside.green(),
    inside.blue(),
    inside.alpha()
  );

  let outside = pixmap.pixel(24, 24).expect("outside pixel");
  assert!(
    outside.green() > 150 && outside.red() < 120 && outside.blue() < 120,
    "expected outer background to remain green, got rgba=({}, {}, {}, {})",
    outside.red(),
    outside.green(),
    outside.blue(),
    outside.alpha()
  );
}

#[test]
fn display_list_embed_renders_html_document() {
  let url = "https://example.test/embed-doc";
  let outer = format!(
    "<!doctype html>\
     <style>\
       html,body{{margin:0;background:rgb(0,200,0);}}\
       embed{{display:block;margin:0;border:0;padding:0;width:16px;height:16px;}}\
     </style>\
     <embed src=\"{url}\">",
  );
  let (pixmap, _calls) = render_with_fetcher("display_list", url, &outer);
  assert_embedded_document_renders(&pixmap);
}

#[test]
fn legacy_embed_renders_html_document() {
  let url = "https://example.test/embed-doc";
  let outer = format!(
    "<!doctype html>\
     <style>\
       html,body{{margin:0;background:rgb(0,200,0);}}\
       embed{{display:block;margin:0;border:0;padding:0;width:16px;height:16px;}}\
     </style>\
     <embed src=\"{url}\">",
  );
  let (pixmap, _calls) = render_with_fetcher("legacy", url, &outer);
  assert_embedded_document_renders(&pixmap);
}

#[test]
fn display_list_object_renders_html_document() {
  let url = "https://example.test/object-doc";
  let outer = format!(
    "<!doctype html>\
     <style>\
       html,body{{margin:0;background:rgb(0,200,0);}}\
       object{{display:block;margin:0;border:0;padding:0;width:16px;height:16px;}}\
     </style>\
     <object data=\"{url}\"></object>",
  );
  let (pixmap, _calls) = render_with_fetcher("display_list", url, &outer);
  assert_embedded_document_renders(&pixmap);
}

#[test]
fn legacy_object_renders_html_document() {
  let url = "https://example.test/object-doc";
  let outer = format!(
    "<!doctype html>\
     <style>\
       html,body{{margin:0;background:rgb(0,200,0);}}\
       object{{display:block;margin:0;border:0;padding:0;width:16px;height:16px;}}\
     </style>\
     <object data=\"{url}\"></object>",
  );
  let (pixmap, _calls) = render_with_fetcher("legacy", url, &outer);
  assert_embedded_document_renders(&pixmap);
}
