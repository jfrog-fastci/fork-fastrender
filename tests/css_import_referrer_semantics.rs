use fastrender::api::{FastRender, FastRenderConfig, RenderDiagnostics, RenderOptions, ResourceContext};
use fastrender::debug::runtime::RuntimeToggles;
use fastrender::error::{Error, Result};
use fastrender::resource::{origin_from_url, FetchDestination, FetchRequest, FetchedResource, ResourceFetcher};
use fastrender::style::media::MediaType;
use image::codecs::png::PngEncoder;
use image::ColorType;
use image::ImageEncoder;
use image::RgbaImage;
use std::collections::HashMap;
use std::io;
use std::sync::{Arc, Mutex};

#[derive(Debug, Clone, PartialEq, Eq)]
struct RecordedRequest {
  url: String,
  destination: FetchDestination,
  referrer_url: Option<String>,
  client_origin: Option<String>,
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

  fn with_font(mut self, url: &str, body: &[u8], final_url: Option<&str>) -> Self {
    self.responses.insert(
      url.to_string(),
      (body.to_vec(), None, final_url.map(|u| u.to_string())),
    );
    self
  }

  fn with_png(mut self, url: &str, bytes: Vec<u8>, final_url: Option<&str>) -> Self {
    self.responses.insert(
      url.to_string(),
      (bytes, Some("image/png".to_string()), final_url.map(|u| u.to_string())),
    );
    self
  }

  fn clear_requests(&self) {
    self
      .requests
      .lock()
      .unwrap_or_else(|poisoned| poisoned.into_inner())
      .clear();
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
        referrer_url: req.referrer_url.map(|r| r.to_string()),
        client_origin: req.client_origin.map(|o| o.to_string()),
      });
    self.fetch(req.url)
  }
}

fn png_bytes() -> Vec<u8> {
  let img = RgbaImage::from_pixel(1, 1, image::Rgba([0, 0, 0, 255]));
  let mut out = Vec::new();
  PngEncoder::new(&mut out)
    .write_image(img.as_raw(), 1, 1, ColorType::Rgba8.into())
    .expect("encode png");
  out
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
  let expected_origin = origin_from_url(document_url)
    .expect("origin")
    .to_string();

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
  assert_eq!(a_request.referrer_url.as_deref(), Some(document_url));
  assert_eq!(
    a_request.client_origin.as_deref(),
    Some(expected_origin.as_str())
  );

  let b_request = requests
    .iter()
    .find(|r| r.url == b_url && r.destination == FetchDestination::Style)
    .expect("request for b.css");
  assert_eq!(b_request.referrer_url.as_deref(), Some(a_url));
  assert_eq!(
    b_request.client_origin.as_deref(),
    Some(expected_origin.as_str())
  );
}

#[test]
fn css_imports_use_stylesheet_final_url_for_base_and_referrer() {
  let document_url = "https://example.test/page.html";
  let html = r#"<html><head><link rel="stylesheet" href="css/a.css"></head><body></body></html>"#;
  let expected_origin = origin_from_url(document_url)
    .expect("origin")
    .to_string();

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
  assert_eq!(a_request.referrer_url.as_deref(), Some(document_url));
  assert_eq!(
    a_request.client_origin.as_deref(),
    Some(expected_origin.as_str())
  );

  let b_request = requests
    .iter()
    .find(|r| r.url == b_expected && r.destination == FetchDestination::Style)
    .expect("request for b.css");
  assert_eq!(b_request.referrer_url.as_deref(), Some(a_final_url));
  assert_eq!(
    b_request.client_origin.as_deref(),
    Some(expected_origin.as_str())
  );
  assert!(
    requests.iter().all(|r| r.url != b_wrong),
    "expected b.css to resolve against final_url, got requests: {requests:?}"
  );
}

#[test]
fn css_import_from_inline_style_uses_document_referrer_even_with_base_href() {
  let document_url = "https://example.test/page.html";
  let base_href = "https://cdn.example.test/assets/";
  let imported_url = "https://cdn.example.test/assets/import.css";
  let expected_origin = origin_from_url(document_url)
    .expect("origin")
    .to_string();

  let fetcher = Arc::new(RecordingFetcher::default().with_css(
    imported_url,
    "body { color: rgb(1, 2, 3); }",
    None,
  ));

  let mut renderer = renderer_for(fetcher.clone());
  renderer
    .render_html_with_stylesheets(
      &format!(
        r#"<!doctype html><html><head>
        <base href="{base_href}">
        <style>@import "import.css";</style>
      </head><body>Hi</body></html>"#
      ),
      document_url,
      RenderOptions::new().with_viewport(16, 16),
    )
    .expect("render");

  let requests = fetcher.requests();
  let import_request = requests
    .iter()
    .find(|r| r.url == imported_url && r.destination == FetchDestination::Style)
    .expect("request for imported stylesheet");
  assert_eq!(import_request.referrer_url.as_deref(), Some(document_url));
  assert_eq!(
    import_request.client_origin.as_deref(),
    Some(expected_origin.as_str())
  );
}

#[test]
fn collect_document_stylesheet_inline_import_uses_document_referrer_even_with_base_href() {
  let document_url = "https://example.test/page.html";
  let base_href = "https://cdn.example.test/assets/";
  let imported_url = "https://cdn.example.test/assets/import.css";
  let expected_origin = origin_from_url(document_url)
    .expect("origin")
    .to_string();

  let fetcher = Arc::new(RecordingFetcher::default().with_css(
    imported_url,
    "body { color: rgb(1, 2, 3); }",
    None,
  ));

  let mut renderer = renderer_for(fetcher.clone());

  // Ensure `FastRender::document_url` is populated (it's distinct from the base URL, which may be
  // overridden via `<base href>`).
  renderer
    .render_html_with_stylesheets(
      "<!doctype html><html><body>init</body></html>",
      document_url,
      RenderOptions::new().with_viewport(1, 1),
    )
    .expect("render init document");
  fetcher.clear_requests();

  renderer.set_base_url(base_href);

  let dom = renderer
    .parse_html(r#"<!doctype html><html><head><style>@import "import.css";</style></head><body></body></html>"#)
    .expect("parse HTML");
  let media_ctx = fastrender::style::media::MediaContext::screen(16.0, 16.0);
  let mut media_query_cache = fastrender::style::media::MediaQueryCache::default();
  renderer
    .collect_document_stylesheet(&dom, &media_ctx, &mut media_query_cache, None)
    .expect("collect stylesheet");

  let requests = fetcher.requests();
  let import_request = requests
    .iter()
    .find(|r| r.url == imported_url && r.destination == FetchDestination::Style)
    .expect("request for imported stylesheet");
  assert_eq!(import_request.referrer_url.as_deref(), Some(document_url));
  assert_eq!(
    import_request.client_origin.as_deref(),
    Some(expected_origin.as_str())
  );
}

#[test]
fn css_imports_from_inline_style_use_imported_final_url_for_nested_referrer_and_base() {
  let document_url = "https://example.test/page.html";
  let base_href = "https://example.test/css/";
  let expected_origin = origin_from_url(document_url)
    .expect("origin")
    .to_string();

  let a_url = "https://example.test/css/a.css";
  let a_final_url = "https://cdn.example.test/assets/a-final.css";
  let b_expected = "https://cdn.example.test/assets/b.css";
  let b_wrong = "https://example.test/css/b.css";

  let fetcher = Arc::new(
    RecordingFetcher::default()
      .with_css(a_url, r#"@import "b.css";"#, Some(a_final_url))
      .with_css(b_expected, "body { background: blue; }", None)
      .with_css(b_wrong, "body { background: red; }", None),
  );

  let mut renderer = renderer_for(fetcher.clone());
  renderer
    .render_html_with_stylesheets(
      &format!(
        r#"<!doctype html><html><head>
        <base href="{base_href}">
        <style>@import "a.css";</style>
      </head><body>Hi</body></html>"#
      ),
      document_url,
      RenderOptions::new().with_viewport(16, 16),
    )
    .expect("render");

  let requests = fetcher.requests();
  let a_request = requests
    .iter()
    .find(|r| r.url == a_url && r.destination == FetchDestination::Style)
    .expect("request for a.css");
  assert_eq!(a_request.referrer_url.as_deref(), Some(document_url));
  assert_eq!(
    a_request.client_origin.as_deref(),
    Some(expected_origin.as_str())
  );

  let b_request = requests
    .iter()
    .find(|r| r.url == b_expected && r.destination == FetchDestination::Style)
    .expect("request for b.css");
  assert_eq!(b_request.referrer_url.as_deref(), Some(a_final_url));
  assert_eq!(
    b_request.client_origin.as_deref(),
    Some(expected_origin.as_str())
  );
  assert!(
    requests.iter().all(|r| r.url != b_wrong),
    "expected b.css to resolve against final_url, got requests: {requests:?}"
  );
}

#[test]
fn link_stylesheet_uses_document_referrer_even_with_base_href() {
  let document_url = "https://example.test/page.html";
  let base_href = "https://cdn.example.test/assets/";
  let stylesheet_url = "https://cdn.example.test/assets/style.css";
  let expected_origin = origin_from_url(document_url)
    .expect("origin")
    .to_string();

  let fetcher = Arc::new(RecordingFetcher::default().with_css(
    stylesheet_url,
    "body { color: rgb(10, 20, 30); }",
    None,
  ));

  let mut renderer = renderer_for(fetcher.clone());
  renderer
    .render_html_with_stylesheets(
      &format!(
        r#"<!doctype html><html><head>
        <base href="{base_href}">
        <link rel="stylesheet" href="style.css">
      </head><body>Hi</body></html>"#
      ),
      document_url,
      RenderOptions::new().with_viewport(16, 16),
    )
    .expect("render");

  let requests = fetcher.requests();
  let sheet_request = requests
    .iter()
    .find(|r| r.url == stylesheet_url && r.destination == FetchDestination::Style)
    .expect("request for stylesheet");
  assert_eq!(sheet_request.referrer_url.as_deref(), Some(document_url));
  assert_eq!(
    sheet_request.client_origin.as_deref(),
    Some(expected_origin.as_str())
  );
}

#[test]
fn inline_stylesheets_for_document_resolves_base_href_without_changing_referrer() {
  let document_url = "https://example.test/page.html";
  let stylesheet_url = "https://example.test/static/style.css";
  let html = r#"<html><head><base href="static/"><link rel="stylesheet" href="style.css"></head><body></body></html>"#;
  let expected_origin = origin_from_url(document_url)
    .expect("origin")
    .to_string();

  let fetcher = Arc::new(RecordingFetcher::default().with_css(
    stylesheet_url,
    "body { color: rgb(10, 20, 30); }",
    None,
  ));
  let renderer = renderer_for(fetcher.clone());

  let mut diagnostics = RenderDiagnostics::default();
  renderer
    .inline_stylesheets_for_document(
      html,
      document_url,
      MediaType::Screen,
      None,
      &mut diagnostics,
      None,
    )
    .expect("inline stylesheets");

  assert!(
    diagnostics.fetch_errors.is_empty(),
    "expected stylesheet fetch to succeed: {:?}",
    diagnostics.fetch_errors
  );
  let requests = fetcher.requests();
  let sheet_request = requests
    .iter()
    .find(|r| r.url == stylesheet_url && r.destination == FetchDestination::Style)
    .expect("request for stylesheet");
  assert_eq!(sheet_request.referrer_url.as_deref(), Some(document_url));
  assert_eq!(
    sheet_request.client_origin.as_deref(),
    Some(expected_origin.as_str())
  );
}

#[test]
fn font_face_from_inline_style_uses_document_referrer_even_with_base_href() {
  let document_url = "https://example.test/page.html";
  let base_href = "https://cdn.example.test/assets/";
  let font_url = "https://cdn.example.test/assets/font.woff2";
  let expected_origin = origin_from_url(document_url)
    .expect("origin")
    .to_string();

  let fetcher = Arc::new(RecordingFetcher::default().with_font(font_url, b"", None));

  let mut renderer = renderer_for(fetcher.clone());
  renderer
    .render_html_with_stylesheets(
      &format!(
        r#"<!doctype html><html><head>
        <base href="{base_href}">
        <style>
          @font-face {{
            font-family: TestFont;
            src: url("font.woff2");
            font-display: block;
          }}
          body {{ font-family: TestFont; }}
        </style>
      </head><body>Hello</body></html>"#
      ),
      document_url,
      RenderOptions::new().with_viewport(16, 16),
    )
    .expect("render");

  let requests = fetcher.requests();
  let font_request = requests
    .iter()
    .find(|r| r.url == font_url && r.destination == FetchDestination::Font)
    .expect("request for font");
  assert_eq!(font_request.referrer_url.as_deref(), Some(document_url));
  assert_eq!(font_request.client_origin.as_deref(), Some(expected_origin.as_str()));
}

#[test]
fn font_face_from_linked_stylesheet_uses_stylesheet_final_url_as_referrer() {
  let document_url = "https://example.test/page.html";
  let base_href = "https://cdn.example.test/assets/";

  let stylesheet_url = "https://cdn.example.test/assets/style.css";
  let stylesheet_final_url = "https://static.other.test/v2/style.css";
  let font_url = "https://static.other.test/v2/font.woff2";
  let expected_origin = origin_from_url(document_url)
    .expect("origin")
    .to_string();

  let css = r#"
    @font-face {
      font-family: TestFont;
      src: url("font.woff2");
      font-display: block;
    }
    body { font-family: TestFont; }
  "#;

  let fetcher = Arc::new(
    RecordingFetcher::default()
      .with_css(stylesheet_url, css, Some(stylesheet_final_url))
      .with_font(font_url, b"", None),
  );

  let mut renderer = renderer_for(fetcher.clone());
  renderer
    .render_html_with_stylesheets(
      &format!(
        r#"<!doctype html><html><head>
        <base href="{base_href}">
        <link rel="stylesheet" href="style.css">
      </head><body>Hello</body></html>"#
      ),
      document_url,
      RenderOptions::new().with_viewport(16, 16),
    )
    .expect("render");

  let requests = fetcher.requests();
  let font_request = requests
    .iter()
    .find(|r| r.url == font_url && r.destination == FetchDestination::Font)
    .expect("request for font");
  assert_eq!(
    font_request.referrer_url.as_deref(),
    Some(stylesheet_final_url),
    "expected font request referrer to be the stylesheet final URL"
  );
  assert_eq!(font_request.client_origin.as_deref(), Some(expected_origin.as_str()));
}

#[test]
fn font_face_from_imported_stylesheet_uses_import_url_as_referrer() {
  let document_url = "https://example.test/page.html";
  let entry_css = "https://example.test/css/entry.css";
  let import_css = "https://example.test/css/fonts.css";
  let font_url = "https://example.test/css/font.woff2";
  let expected_origin = origin_from_url(document_url)
    .expect("origin")
    .to_string();

  let fetcher = Arc::new(
    RecordingFetcher::default()
      .with_css(entry_css, r#"@import "fonts.css";"#, None)
      .with_css(
        import_css,
        r#"
          @font-face {
            font-family: TestFont;
            src: url("font.woff2");
            font-display: block;
          }
          body { font-family: TestFont; }
        "#,
        None,
      )
      .with_font(font_url, b"", None),
  );

  let mut renderer = renderer_for(fetcher.clone());
  renderer
    .render_html_with_stylesheets(
      &format!(
        r#"<!doctype html><html><head>
        <link rel="stylesheet" href="{entry_css}">
      </head><body>Hello</body></html>"#
      ),
      document_url,
      RenderOptions::new().with_viewport(16, 16),
    )
    .expect("render");

  let requests = fetcher.requests();
  let font_request = requests
    .iter()
    .find(|r| r.url == font_url && r.destination == FetchDestination::Font)
    .expect("request for font");
  assert_eq!(
    font_request.referrer_url.as_deref(),
    Some(import_css),
    "expected font request referrer to be the imported stylesheet URL"
  );
  assert_eq!(font_request.client_origin.as_deref(), Some(expected_origin.as_str()));
}

#[test]
fn css_background_image_from_inline_style_uses_document_referrer_even_with_base_href() {
  let document_url = "https://example.test/page.html";
  let base_href = "https://cdn.example.test/assets/";
  let image_url = "https://cdn.example.test/assets/img.png";
  let expected_origin = origin_from_url(document_url)
    .expect("origin")
    .to_string();

  let fetcher = Arc::new(
    RecordingFetcher::default().with_png(image_url, png_bytes(), None),
  );

  let mut renderer = renderer_for(fetcher.clone());
  renderer
    .render_html_with_stylesheets(
      &format!(
        r#"<!doctype html><html><head>
        <base href="{base_href}">
        <style>
          body {{ margin: 0; }}
          #target {{ width: 1px; height: 1px; background: url("img.png") no-repeat; }}
        </style>
      </head><body><div id="target"></div></body></html>"#
      ),
      document_url,
      RenderOptions::new().with_viewport(16, 16),
    )
    .expect("render");

  let requests = fetcher.requests();
  let image_request = requests
    .iter()
    .find(|r| r.url == image_url && r.destination == FetchDestination::Image)
    .expect("request for background image");
  assert_eq!(image_request.referrer_url.as_deref(), Some(document_url));
  assert_eq!(
    image_request.client_origin.as_deref(),
    Some(expected_origin.as_str())
  );
}
