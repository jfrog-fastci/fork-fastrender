use fastrender::api::{FastRender, FastRenderConfig, RenderOptions};
use fastrender::debug::runtime::RuntimeToggles;
use fastrender::error::{Error, Result};
use fastrender::resource::{origin_from_url, DocumentOrigin, FetchDestination, FetchRequest, FetchedResource, ResourceFetcher};
use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::Path;
use std::sync::{Arc, Mutex};

#[derive(Debug, Clone, PartialEq, Eq)]
struct RecordedRequest {
  url: String,
  destination: FetchDestination,
  referrer_url: Option<String>,
  client_origin: Option<DocumentOrigin>,
}

#[derive(Default)]
struct RecordingRequestFetcher {
  responses: HashMap<String, (Vec<u8>, Option<String>, Option<String>)>,
  requests: Arc<Mutex<Vec<RecordedRequest>>>,
}

impl RecordingRequestFetcher {
  fn with_entry(mut self, url: &str, bytes: Vec<u8>, content_type: &str) -> Self {
    self
      .responses
      .insert(url.to_string(), (bytes, Some(content_type.to_string()), None));
    self
  }

  fn with_entry_with_final_url(
    mut self,
    url: &str,
    bytes: Vec<u8>,
    content_type: &str,
    final_url: &str,
  ) -> Self {
    self.responses.insert(
      url.to_string(),
      (
        bytes,
        Some(content_type.to_string()),
        Some(final_url.to_string()),
      ),
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
    let Some((bytes, content_type, final_url)) = self.responses.get(url) else {
      return Err(Error::Io(io::Error::new(
        io::ErrorKind::NotFound,
        format!("missing resource: {url}"),
      )));
    };
    let mut resource = FetchedResource::new(bytes.clone(), content_type.clone());
    resource.final_url = final_url.clone();
    Ok(resource)
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
        client_origin: req.client_origin.cloned(),
      });
    self.fetch(req.url)
  }
}

#[test]
fn web_font_fetch_uses_stylesheet_referrer_but_document_origin() {
  let font_path = Path::new("tests/fixtures/fonts/DejaVuSans-subset.ttf");
  let font_bytes = fs::read(font_path).expect("read fixture font bytes");

  let document_url = "https://a.test/page";
  let stylesheet_url = "https://b.test/style.css";
  let font_url = "https://b.test/font.ttf";
  let css = format!(
    r#"
@font-face {{
  font-family: "TestFace";
  src: url("{font_url}");
  font-display: block;
}}
body {{ font-family: "TestFace"; }}
"#
  );

  let fetcher = Arc::new(
    RecordingRequestFetcher::default()
      .with_entry(stylesheet_url, css.into_bytes(), "text/css")
      .with_entry(font_url, font_bytes, "font/ttf"),
  );

  let toggles = RuntimeToggles::from_map(HashMap::from([
    ("FASTR_FETCH_LINK_CSS".to_string(), "1".to_string()),
    ("FASTR_FETCH_ENFORCE_CORS".to_string(), "0".to_string()),
  ]));
  let config = FastRenderConfig::default().with_runtime_toggles(toggles);
  let mut renderer =
    FastRender::with_config_and_fetcher(config, Some(fetcher.clone() as Arc<dyn ResourceFetcher>))
      .unwrap();

  renderer
    .render_html_with_stylesheets(
      &format!(
        r#"<!doctype html><html><head>
        <link rel="stylesheet" href="{stylesheet_url}">
      </head><body>Hi</body></html>"#
      ),
      document_url,
      RenderOptions::new().with_viewport(64, 64),
    )
    .unwrap();

  let expected_origin = origin_from_url(document_url).expect("origin");

  let requests = fetcher.requests();
  let font_request = requests
    .iter()
    .find(|request| request.destination == FetchDestination::Font)
    .expect("font request");

  assert_eq!(font_request.url, font_url);
  assert_eq!(font_request.referrer_url.as_deref(), Some(stylesheet_url));
  assert_eq!(font_request.client_origin.as_ref(), Some(&expected_origin));
}

#[test]
fn web_font_fetch_uses_imported_stylesheet_referrer_but_document_origin() {
  let font_path = Path::new("tests/fixtures/fonts/DejaVuSans-subset.ttf");
  let font_bytes = fs::read(font_path).expect("read fixture font bytes");

  let document_url = "https://a.test/page";
  let stylesheet_url = "https://b.test/style.css";
  let imported_url = "https://b.test/imported.css";
  let imported_final_url = "https://c.test/final.css";
  let font_url = "https://c.test/font.ttf";
  let css_root = r#"@import "imported.css";"#.to_string();
  let css_imported = format!(
    r#"
@font-face {{
  font-family: "TestFace";
  src: url("font.ttf");
  font-display: block;
}}
body {{ font-family: "TestFace"; }}
"#
  );

  let fetcher = Arc::new(
    RecordingRequestFetcher::default()
      .with_entry(stylesheet_url, css_root.into_bytes(), "text/css")
      .with_entry_with_final_url(
        imported_url,
        css_imported.into_bytes(),
        "text/css",
        imported_final_url,
      )
      .with_entry(font_url, font_bytes, "font/ttf"),
  );

  let toggles = RuntimeToggles::from_map(HashMap::from([
    ("FASTR_FETCH_LINK_CSS".to_string(), "1".to_string()),
    ("FASTR_FETCH_ENFORCE_CORS".to_string(), "0".to_string()),
  ]));
  let config = FastRenderConfig::default().with_runtime_toggles(toggles);
  let mut renderer =
    FastRender::with_config_and_fetcher(config, Some(fetcher.clone() as Arc<dyn ResourceFetcher>))
      .unwrap();

  renderer
    .render_html_with_stylesheets(
      &format!(
        r#"<!doctype html><html><head>
        <link rel="stylesheet" href="{stylesheet_url}">
      </head><body>Hi</body></html>"#
      ),
      document_url,
      RenderOptions::new().with_viewport(64, 64),
    )
    .unwrap();

  let expected_origin = origin_from_url(document_url).expect("origin");

  let requests = fetcher.requests();
  let font_request = requests
    .iter()
    .find(|request| request.destination == FetchDestination::Font)
    .expect("font request");

  assert_eq!(font_request.url, font_url);
  assert_eq!(
    font_request.referrer_url.as_deref(),
    Some(imported_final_url)
  );
  assert_eq!(font_request.client_origin.as_ref(), Some(&expected_origin));
}
