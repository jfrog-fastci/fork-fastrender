use fastrender::api::{FastRender, FastRenderConfig, RenderOptions};
use fastrender::debug::runtime::RuntimeToggles;
use fastrender::error::{Error, Result};
use fastrender::resource::{
  FetchDestination, FetchRequest, FetchedResource, ReferrerPolicy, ResourceFetcher,
};
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
  referrer_policy: ReferrerPolicy,
}

#[derive(Default)]
struct RecordingFetcher {
  responses: HashMap<String, FetchedResource>,
  requests: Mutex<Vec<RecordedRequest>>,
}

impl RecordingFetcher {
  fn with_response(mut self, url: &str, res: FetchedResource) -> Self {
    self.responses.insert(url.to_string(), res);
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

fn fixture_font_bytes() -> Option<Vec<u8>> {
  let path = Path::new("tests/fixtures/fonts/DejaVuSans-subset.ttf");
  fs::read(path).ok()
}

impl ResourceFetcher for RecordingFetcher {
  fn fetch(&self, _url: &str) -> Result<FetchedResource> {
    panic!("expected stylesheet fetches to use fetch_with_request()");
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
        referrer_policy: req.referrer_policy,
      });

    self
      .responses
      .get(req.url)
      .cloned()
      .ok_or_else(|| Error::Io(io::Error::new(io::ErrorKind::NotFound, format!("missing {req:?}"))))
  }
}

#[test]
fn stylesheet_referrer_policy_header_applies_to_import_requests() {
  let document_url = "https://doc.test/page.html";
  let stylesheet_url = "https://assets.test/style.css";
  let import_url = "https://assets.test/import.css";

  let mut stylesheet = FetchedResource::with_final_url(
    format!(r#"@import url("{import_url}"); body {{ color: rgb(1, 2, 3); }}"#).into_bytes(),
    Some("text/css".to_string()),
    Some(stylesheet_url.to_string()),
  );
  stylesheet.status = Some(200);
  stylesheet.response_referrer_policy = Some(ReferrerPolicy::NoReferrer);

  let mut imported = FetchedResource::with_final_url(
    b"body { background: rgb(0, 0, 0); }".to_vec(),
    Some("text/css".to_string()),
    Some(import_url.to_string()),
  );
  imported.status = Some(200);

  let fetcher = Arc::new(
    RecordingFetcher::default()
      .with_response(stylesheet_url, stylesheet)
      .with_response(import_url, imported),
  );

  let toggles = RuntimeToggles::from_map(HashMap::from([(
    "FASTR_FETCH_LINK_CSS".to_string(),
    "1".to_string(),
  )]));
  let config = FastRenderConfig::default().with_runtime_toggles(toggles);
  let mut renderer = FastRender::with_config_and_fetcher(
    config,
    Some(fetcher.clone() as Arc<dyn ResourceFetcher>),
  )
  .unwrap();

  renderer
    .render_html_with_stylesheets(
      &format!(
        r#"<!doctype html><html><head>
          <link rel="stylesheet" href="{stylesheet_url}">
        </head><body>ok</body></html>"#
      ),
      document_url,
      RenderOptions::new().with_viewport(64, 64),
    )
    .unwrap();

  let requests = fetcher.requests();
  let import_request = requests
    .iter()
    .find(|request| request.url == import_url && request.destination == FetchDestination::Style)
    .expect("expected @import stylesheet fetch");

  assert_eq!(import_request.referrer_url.as_deref(), Some(stylesheet_url));
  assert_eq!(import_request.referrer_policy, ReferrerPolicy::NoReferrer);
}

#[test]
fn nested_stylesheet_referrer_policy_headers_override_for_grandchild_imports() {
  let document_url = "https://doc.test/page.html";
  let stylesheet_url = "https://assets.test/style.css";
  let first_import_url = "https://assets.test/first.css";
  let first_final_url = "https://cdn.test/first-final.css";
  let second_import_url = "https://cdn.test/second.css";
  let second_wrong_url = "https://assets.test/second.css";

  let mut stylesheet = FetchedResource::with_final_url(
    b"@import url('first.css'); body { color: rgb(1, 2, 3); }".to_vec(),
    Some("text/css".to_string()),
    Some(stylesheet_url.to_string()),
  );
  stylesheet.status = Some(200);
  stylesheet.response_referrer_policy = Some(ReferrerPolicy::Origin);

  let mut first = FetchedResource::with_final_url(
    b"@import url('second.css'); body { background: rgb(0, 0, 0); }".to_vec(),
    Some("text/css".to_string()),
    Some(first_final_url.to_string()),
  );
  first.status = Some(200);
  first.response_referrer_policy = Some(ReferrerPolicy::NoReferrer);

  let mut second = FetchedResource::with_final_url(
    b"body { background: rgb(4, 5, 6); }".to_vec(),
    Some("text/css".to_string()),
    Some(second_import_url.to_string()),
  );
  second.status = Some(200);

  let mut second_wrong = FetchedResource::with_final_url(
    b"body { background: rgb(255, 0, 0); }".to_vec(),
    Some("text/css".to_string()),
    Some(second_wrong_url.to_string()),
  );
  second_wrong.status = Some(200);

  let fetcher = Arc::new(
    RecordingFetcher::default()
      .with_response(stylesheet_url, stylesheet)
      .with_response(
        first_import_url,
        // `first.css` redirects to a different base URL; subsequent imports should use that final
        // URL as both the base and the referrer. Its `Referrer-Policy` response header should
        // override the policy used for any nested `@import` fetches.
        first,
      )
      .with_response(second_import_url, second)
      .with_response(second_wrong_url, second_wrong),
  );

  let toggles = RuntimeToggles::from_map(HashMap::from([(
    "FASTR_FETCH_LINK_CSS".to_string(),
    "1".to_string(),
  )]));
  let config = FastRenderConfig::default().with_runtime_toggles(toggles);
  let mut renderer = FastRender::with_config_and_fetcher(
    config,
    Some(fetcher.clone() as Arc<dyn ResourceFetcher>),
  )
  .unwrap();

  renderer
    .render_html_with_stylesheets(
      &format!(
        r#"<!doctype html><html><head>
          <link rel="stylesheet" href="{stylesheet_url}">
        </head><body>ok</body></html>"#
      ),
      document_url,
      RenderOptions::new().with_viewport(64, 64),
    )
    .unwrap();

  let requests = fetcher.requests();
  let first_request = requests
    .iter()
    .find(|request| request.url == first_import_url && request.destination == FetchDestination::Style)
    .expect("expected first @import stylesheet fetch");
  assert_eq!(first_request.referrer_url.as_deref(), Some(stylesheet_url));
  assert_eq!(first_request.referrer_policy, ReferrerPolicy::Origin);

  let second_request = requests
    .iter()
    .find(|request| request.url == second_import_url && request.destination == FetchDestination::Style)
    .expect("expected nested @import stylesheet fetch");
  assert_eq!(second_request.referrer_url.as_deref(), Some(first_final_url));
  assert_eq!(second_request.referrer_policy, ReferrerPolicy::NoReferrer);

  assert!(
    requests.iter().all(|req| req.url != second_wrong_url),
    "expected nested import to resolve against first stylesheet final URL; got requests: {requests:?}"
  );
}

#[test]
fn stylesheet_referrer_policy_header_applies_to_font_requests() {
  let font_bytes = match fixture_font_bytes() {
    Some(bytes) => bytes,
    None => return,
  };

  let document_url = "https://doc.test/page.html";
  let stylesheet_url = "https://assets.test/style.css";
  let font_url = "https://fonts.test/font.ttf";

  let mut stylesheet = FetchedResource::with_final_url(
    format!(
      r#"@font-face {{ font-family: "PolicyFont"; src: url("{font_url}"); }}
      body {{ color: rgb(1, 2, 3); }}"#
    )
    .into_bytes(),
    Some("text/css".to_string()),
    Some(stylesheet_url.to_string()),
  );
  stylesheet.status = Some(200);
  stylesheet.response_referrer_policy = Some(ReferrerPolicy::NoReferrer);

  let mut font = FetchedResource::with_final_url(
    font_bytes,
    Some("font/ttf".to_string()),
    Some(font_url.to_string()),
  );
  font.status = Some(200);

  let fetcher = Arc::new(
    RecordingFetcher::default()
      .with_response(stylesheet_url, stylesheet)
      .with_response(font_url, font),
  );

  let toggles = RuntimeToggles::from_map(HashMap::from([(
    "FASTR_FETCH_LINK_CSS".to_string(),
    "1".to_string(),
  )]));
  let config = FastRenderConfig::default().with_runtime_toggles(toggles);
  let mut renderer = FastRender::with_config_and_fetcher(
    config,
    Some(fetcher.clone() as Arc<dyn ResourceFetcher>),
  )
  .unwrap();

  renderer
    .render_html_with_stylesheets(
      &format!(
        r#"<!doctype html><html><head>
          <link rel="stylesheet" href="{stylesheet_url}">
        </head><body>ok</body></html>"#
      ),
      document_url,
      RenderOptions::new().with_viewport(64, 64),
    )
    .unwrap();

  let requests = fetcher.requests();
  let font_request = requests
    .iter()
    .find(|request| request.url == font_url && request.destination == FetchDestination::Font)
    .expect("expected font fetch from @font-face stylesheet");

  assert_eq!(font_request.referrer_url.as_deref(), Some(stylesheet_url));
  assert_eq!(font_request.referrer_policy, ReferrerPolicy::NoReferrer);
}

#[test]
fn nested_stylesheet_referrer_policy_headers_override_for_font_requests() {
  let font_bytes = match fixture_font_bytes() {
    Some(bytes) => bytes,
    None => return,
  };

  let document_url = "https://doc.test/page.html";
  let root_stylesheet_url = "https://assets.test/style.css";
  let import_url = "https://assets.test/import.css";
  let import_final_url = "https://cdn.test/import-final.css";
  let font_url = "https://fonts.test/nested.ttf";

  let mut root = FetchedResource::with_final_url(
    b"@import url('import.css'); body { color: rgb(1, 2, 3); }".to_vec(),
    Some("text/css".to_string()),
    Some(root_stylesheet_url.to_string()),
  );
  root.status = Some(200);
  root.response_referrer_policy = Some(ReferrerPolicy::Origin);

  let mut imported = FetchedResource::with_final_url(
    format!(
      r#"@font-face {{ font-family: "NestedFont"; src: url("{font_url}"); }}
      body {{ background: rgb(0, 0, 0); }}"#
    )
    .into_bytes(),
    Some("text/css".to_string()),
    Some(import_final_url.to_string()),
  );
  imported.status = Some(200);
  imported.response_referrer_policy = Some(ReferrerPolicy::NoReferrer);

  let mut font = FetchedResource::with_final_url(
    font_bytes,
    Some("font/ttf".to_string()),
    Some(font_url.to_string()),
  );
  font.status = Some(200);

  let fetcher = Arc::new(
    RecordingFetcher::default()
      .with_response(root_stylesheet_url, root)
      .with_response(import_url, imported)
      .with_response(font_url, font),
  );

  let toggles = RuntimeToggles::from_map(HashMap::from([(
    "FASTR_FETCH_LINK_CSS".to_string(),
    "1".to_string(),
  )]));
  let config = FastRenderConfig::default().with_runtime_toggles(toggles);
  let mut renderer = FastRender::with_config_and_fetcher(
    config,
    Some(fetcher.clone() as Arc<dyn ResourceFetcher>),
  )
  .unwrap();

  renderer
    .render_html_with_stylesheets(
      &format!(
        r#"<!doctype html><html><head>
          <link rel="stylesheet" href="{root_stylesheet_url}">
        </head><body>ok</body></html>"#
      ),
      document_url,
      RenderOptions::new().with_viewport(64, 64),
    )
    .unwrap();

  let requests = fetcher.requests();
  let font_request = requests
    .iter()
    .find(|request| request.url == font_url && request.destination == FetchDestination::Font)
    .expect("expected nested @font-face font fetch");

  assert_eq!(font_request.referrer_url.as_deref(), Some(import_final_url));
  assert_eq!(font_request.referrer_policy, ReferrerPolicy::NoReferrer);
}
