use fastrender::api::{FastRender, FastRenderConfig, RenderDiagnostics, ResourceContext};
use fastrender::debug::runtime::RuntimeToggles;
use fastrender::error::{Error, Result};
use fastrender::resource::{FetchDestination, FetchRequest, FetchedResource, ResourceFetcher};
use fastrender::style::media::MediaType;
use std::collections::HashMap;
use std::io;
use std::sync::{Arc, Mutex};

#[derive(Debug, Clone, PartialEq, Eq)]
struct RecordedRequest {
  url: String,
  destination: FetchDestination,
  referrer: Option<String>,
}

#[derive(Default)]
struct RecordingFetcher {
  responses: HashMap<String, (Vec<u8>, Option<String>, Option<String>)>,
  requests: Arc<Mutex<Vec<RecordedRequest>>>,
}

impl RecordingFetcher {
  fn with_css(mut self, url: &str, body: &str, final_url: Option<&str>) -> Self {
    self.responses.insert(
      url.to_string(),
      (
        body.as_bytes().to_vec(),
        Some("text/css".to_string()),
        final_url.map(|u| u.to_string()),
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

impl ResourceFetcher for RecordingFetcher {
  fn fetch(&self, url: &str) -> Result<FetchedResource> {
    let Some((bytes, content_type, final_url)) = self.responses.get(url) else {
      return Err(Error::Io(io::Error::new(
        io::ErrorKind::NotFound,
        format!("missing mocked resource: {url}"),
      )));
    };
    Ok(FetchedResource::with_final_url(
      bytes.clone(),
      content_type.clone(),
      final_url.clone(),
    ))
  }

  fn fetch_with_request(&self, req: FetchRequest<'_>) -> Result<FetchedResource> {
    self
      .requests
      .lock()
      .unwrap_or_else(|poisoned| poisoned.into_inner())
      .push(RecordedRequest {
        url: req.url.to_string(),
        destination: req.destination,
        referrer: req.referrer.map(|r| r.to_string()),
      });
    self.fetch(req.url)
  }
}

fn renderer_for(fetcher: Arc<RecordingFetcher>) -> FastRender {
  let toggles = RuntimeToggles::from_map(HashMap::from([(
    "FASTR_FETCH_LINK_CSS".to_string(),
    "1".to_string(),
  )]));
  let config = FastRenderConfig::default().with_runtime_toggles(toggles);
  FastRender::with_config_and_fetcher(config, Some(fetcher as Arc<dyn ResourceFetcher>))
    .expect("renderer should build")
}

#[test]
fn css_import_referrer_is_importing_stylesheet_url() {
  let document_url = "https://example.test/page.html";
  let html = r#"<html><head><link rel="stylesheet" href="a.css"></head><body></body></html>"#;

  let a_url = "https://example.test/a.css";
  let b_url = "https://example.test/b.css";

  let fetcher = Arc::new(
    RecordingFetcher::default()
      .with_css(a_url, r#"@import "b.css";"#, None)
      .with_css(b_url, "body { color: red; }", None),
  );
  let renderer = renderer_for(fetcher.clone());
  let resource_context = ResourceContext {
    document_url: Some(document_url.to_string()),
    ..Default::default()
  };

  let mut diagnostics = RenderDiagnostics::default();
  renderer
    .inline_stylesheets_for_document_with_context(
      html,
      document_url,
      MediaType::Screen,
      None,
      Some(&resource_context),
      &mut diagnostics,
      None,
    )
    .expect("inline stylesheets");

  let requests = fetcher.requests();
  let a_request = requests
    .iter()
    .find(|r| r.url == a_url && r.destination == FetchDestination::Style)
    .expect("request for a.css");
  assert_eq!(a_request.referrer.as_deref(), Some(document_url));

  let b_request = requests
    .iter()
    .find(|r| r.url == b_url && r.destination == FetchDestination::Style)
    .expect("request for b.css");
  assert_eq!(b_request.referrer.as_deref(), Some(a_url));
}

#[test]
fn css_imports_use_stylesheet_final_url_for_base_and_referrer() {
  let document_url = "https://example.test/page.html";
  let html = r#"<html><head><link rel="stylesheet" href="css/a.css"></head><body></body></html>"#;

  let a_url = "https://example.test/css/a.css";
  let a_final_url = "https://example.test/assets/a-final.css";
  let b_expected = "https://example.test/assets/b.css";
  let b_wrong = "https://example.test/css/b.css";

  let fetcher = Arc::new(
    RecordingFetcher::default()
      .with_css(a_url, r#"@import "b.css";"#, Some(a_final_url))
      .with_css(b_expected, "body { background: blue; }", None)
      .with_css(b_wrong, "body { background: red; }", None),
  );
  let renderer = renderer_for(fetcher.clone());
  let resource_context = ResourceContext {
    document_url: Some(document_url.to_string()),
    ..Default::default()
  };

  let mut diagnostics = RenderDiagnostics::default();
  renderer
    .inline_stylesheets_for_document_with_context(
      html,
      document_url,
      MediaType::Screen,
      None,
      Some(&resource_context),
      &mut diagnostics,
      None,
    )
    .expect("inline stylesheets");

  let requests = fetcher.requests();
  let a_request = requests
    .iter()
    .find(|r| r.url == a_url && r.destination == FetchDestination::Style)
    .expect("request for a.css");
  assert_eq!(a_request.referrer.as_deref(), Some(document_url));

  let b_request = requests
    .iter()
    .find(|r| r.url == b_expected && r.destination == FetchDestination::Style)
    .expect("request for b.css");
  assert_eq!(b_request.referrer.as_deref(), Some(a_final_url));
  assert!(
    requests.iter().all(|r| r.url != b_wrong),
    "expected b.css to resolve against final_url, got requests: {requests:?}"
  );
}

