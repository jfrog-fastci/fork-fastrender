use fastrender::api::{FastRender, FastRenderConfig, RenderOptions};
use fastrender::debug::runtime::RuntimeToggles;
use fastrender::resource::HttpFetcher;
use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

fn try_bind_localhost(context: &str) -> Option<TcpListener> {
  match TcpListener::bind("127.0.0.1:0") {
    Ok(listener) => Some(listener),
    Err(err)
      if matches!(
        err.kind(),
        io::ErrorKind::PermissionDenied | io::ErrorKind::AddrNotAvailable
      ) =>
    {
      eprintln!("skipping {context}: cannot bind localhost in this environment: {err}");
      None
    }
    Err(err) => panic!("bind {context}: {err}"),
  }
}

fn read_http_headers(stream: &mut TcpStream) -> io::Result<String> {
  stream.set_read_timeout(Some(Duration::from_secs(1)))?;
  let mut buf = [0u8; 4096];
  let mut data = Vec::new();
  loop {
    match stream.read(&mut buf) {
      Ok(0) => break,
      Ok(n) => {
        data.extend_from_slice(&buf[..n]);
        if data.windows(4).any(|w| w == b"\r\n\r\n") {
          break;
        }
      }
      Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
      Err(err) => return Err(err),
    }
  }
  Ok(String::from_utf8_lossy(&data).into_owned())
}

fn request_path(headers: &str) -> Option<String> {
  let first_line = headers.lines().next()?;
  let mut parts = first_line.split_whitespace();
  parts.next()?; // method
  let raw_target = parts.next()?;
  // Requests can be either origin-form (`GET /path HTTP/1.1`) or absolute-form
  // (`GET http://host/path HTTP/1.1`). Normalize both into just the path so the
  // assertions can stay stable across HTTP backends.
  Some(match url::Url::parse(raw_target).ok() {
    Some(url) => url.path().to_string(),
    None => raw_target
      .split_once('?')
      .map(|(before, _)| before)
      .unwrap_or(raw_target)
      .to_string(),
  })
}

fn header_value(headers: &str, header_name: &str) -> Option<String> {
  for line in headers.lines() {
    let Some((name, value)) = line.split_once(':') else {
      continue;
    };
    if name.trim().eq_ignore_ascii_case(header_name) {
      return Some(value.trim().to_string());
    }
  }
  None
}

#[derive(Clone)]
struct CapturedRequest {
  path: String,
  headers: String,
}

struct HeaderCaptureServer {
  base_url: String,
  requests: Arc<Mutex<Vec<CapturedRequest>>>,
  shutdown: Arc<AtomicBool>,
  join: Option<thread::JoinHandle<()>>,
}

fn test_woff2_bytes() -> &'static [u8] {
  // Small WOFF2 fixture already checked into the repo (from an offline pageset capture).
  include_bytes!("pages/fixtures/ebay.com/assets/688dde5436f7262e45f65fa87ba02f36.woff2")
}

impl HeaderCaptureServer {
  fn start(context: &str) -> Option<Self> {
    let listener = try_bind_localhost(context)?;
    let addr = listener.local_addr().ok()?;
    let base_url = format!("http://{addr}");
    let requests = Arc::new(Mutex::new(Vec::new()));
    let shutdown = Arc::new(AtomicBool::new(false));

    let thread_requests = Arc::clone(&requests);
    let thread_shutdown = Arc::clone(&shutdown);
    let join = thread::spawn(move || {
      let _ = listener.set_nonblocking(true);
      while !thread_shutdown.load(Ordering::SeqCst) {
        match listener.accept() {
          Ok((mut stream, _)) => {
            let req = read_http_headers(&mut stream).unwrap_or_default();
            let path = request_path(&req).unwrap_or_else(|| "/".to_string());
            if let Ok(mut guard) = thread_requests.lock() {
              guard.push(CapturedRequest {
                path: path.clone(),
                headers: req.clone(),
              });
            }

            let (status, content_type, body) = match path.as_str() {
              "/img.png" => ("200 OK", "image/png", minimal_png().to_vec()),
              "/frame.html" => (
                "200 OK",
                "text/html; charset=utf-8",
                b"<!doctype html><body>frame</body>".to_vec(),
              ),
              "/style.css" => ("200 OK", "text/css; charset=utf-8", b"body { }".to_vec()),
              "/doc_response_policy.html" => (
                "200 OK",
                "text/html; charset=utf-8",
                br#"<!doctype html>
<html>
  <head>
    <link rel="stylesheet" href="/style_import.css">
  </head>
  <body>
    <div>hello</div>
  </body>
</html>"#
                  .to_vec(),
              ),
              "/doc_response_policy_img.html" => (
                "200 OK",
                "text/html; charset=utf-8",
                br#"<!doctype html>
<html>
  <head></head>
  <body>
    <img src="/img.png" style="width: 10px; height: 10px">
  </body>
</html>"#
                  .to_vec(),
              ),
              "/doc_response_policy_img_meta_override.html" => (
                "200 OK",
                "text/html; charset=utf-8",
                br#"<!doctype html>
<html>
  <head>
    <meta name="referrer" content="origin">
  </head>
  <body>
    <img src="/img.png" style="width: 10px; height: 10px">
  </body>
</html>"#
                  .to_vec(),
              ),
              "/doc_response_policy_img_attr_override.html" => (
                "200 OK",
                "text/html; charset=utf-8",
                br#"<!doctype html>
<html>
  <head></head>
  <body>
    <img src="/img.png" referrerpolicy="origin" style="width: 10px; height: 10px">
  </body>
</html>"#
                  .to_vec(),
              ),
              "/doc_response_policy_iframe.html" => (
                "200 OK",
                "text/html; charset=utf-8",
                br#"<!doctype html>
<html>
  <head></head>
  <body>
    <iframe src="/frame.html" style="width: 10px; height: 10px"></iframe>
  </body>
</html>"#
                  .to_vec(),
              ),
              "/doc_response_policy_iframe_meta_override.html" => (
                "200 OK",
                "text/html; charset=utf-8",
                br#"<!doctype html>
<html>
  <head>
    <meta name="referrer" content="origin">
  </head>
  <body>
    <iframe src="/frame.html" style="width: 10px; height: 10px"></iframe>
  </body>
</html>"#
                  .to_vec(),
              ),
              "/doc_response_policy_iframe_attr_override.html" => (
                "200 OK",
                "text/html; charset=utf-8",
                br#"<!doctype html>
<html>
  <head></head>
  <body>
    <iframe src="/frame.html" referrerpolicy="origin" style="width: 10px; height: 10px"></iframe>
  </body>
</html>"#
                  .to_vec(),
              ),
              "/doc_links_style_import.html" => (
                "200 OK",
                "text/html; charset=utf-8",
                br#"<!doctype html>
<html>
  <head>
    <link rel="stylesheet" href="/style_import.css">
  </head>
  <body>
    <div>hello</div>
  </body>
</html>"#
                  .to_vec(),
              ),
              "/doc_links_style_import_link_override.html" => (
                "200 OK",
                "text/html; charset=utf-8",
                br#"<!doctype html>
<html>
  <head>
    <link rel="stylesheet" href="/style_import.css" referrerpolicy="origin">
  </head>
  <body>
    <div>hello</div>
  </body>
</html>"#
                  .to_vec(),
              ),
              "/doc_links_style_import_meta_origin.html" => (
                "200 OK",
                "text/html; charset=utf-8",
                br#"<!doctype html>
<html>
  <head>
    <meta name="referrer" content="origin">
    <link rel="stylesheet" href="/style_import.css">
  </head>
  <body>
    <div>hello</div>
  </body>
</html>"#
                  .to_vec(),
              ),
              "/doc_links_img.html" => (
                "200 OK",
                "text/html; charset=utf-8",
                br#"<!doctype html>
<html>
  <head></head>
  <body>
    <img src="/img.png" style="width: 10px; height: 10px">
  </body>
</html>"#
                  .to_vec(),
              ),
              "/doc_links_iframe.html" => (
                "200 OK",
                "text/html; charset=utf-8",
                br#"<!doctype html>
<html>
  <head></head>
  <body>
    <iframe src="/frame.html" style="width: 10px; height: 10px"></iframe>
  </body>
</html>"#
                  .to_vec(),
              ),
              "/doc_links_img_meta_origin.html" => (
                "200 OK",
                "text/html; charset=utf-8",
                br#"<!doctype html>
<html>
  <head>
    <meta name="referrer" content="origin">
  </head>
  <body>
    <img src="/img.png" style="width: 10px; height: 10px">
  </body>
</html>"#
                  .to_vec(),
              ),
              "/doc_links_iframe_meta_origin.html" => (
                "200 OK",
                "text/html; charset=utf-8",
                br#"<!doctype html>
<html>
  <head>
    <meta name="referrer" content="origin">
  </head>
  <body>
    <iframe src="/frame.html" style="width: 10px; height: 10px"></iframe>
  </body>
</html>"#
                  .to_vec(),
              ),
              "/doc_response_policy_link_override.html" => (
                "200 OK",
                "text/html; charset=utf-8",
                br#"<!doctype html>
<html>
  <head>
    <link rel="stylesheet" href="/style_import.css" referrerpolicy="origin">
  </head>
  <body>
    <div>hello</div>
  </body>
</html>"#
                  .to_vec(),
              ),
              "/doc_meta_policy.html" => (
                "200 OK",
                "text/html; charset=utf-8",
                br#"<!doctype html>
<html>
  <head>
    <meta name="referrer" content="no-referrer">
    <link rel="stylesheet" href="/style_import.css">
  </head>
  <body>
    <div>hello</div>
  </body>
</html>"#
                  .to_vec(),
              ),
              "/doc_response_policy_meta_override.html" => (
                "200 OK",
                "text/html; charset=utf-8",
                br#"<!doctype html>
<html>
  <head>
    <meta name="referrer" content="origin">
    <link rel="stylesheet" href="/style_import.css">
  </head>
  <body>
    <div>hello</div>
  </body>
</html>"#
                  .to_vec(),
              ),
              "/doc_redirect_policy.html" => (
                "302 Found",
                "text/plain; charset=utf-8",
                b"redirecting".to_vec(),
              ),
              "/doc_redirect_policy_img.html" => (
                "302 Found",
                "text/plain; charset=utf-8",
                b"redirecting".to_vec(),
              ),
              "/doc_redirect_policy_iframe.html" => (
                "302 Found",
                "text/plain; charset=utf-8",
                b"redirecting".to_vec(),
              ),
              "/doc_redirect_policy_link_override.html" => (
                "302 Found",
                "text/plain; charset=utf-8",
                b"redirecting".to_vec(),
              ),
              "/doc_redirect_policy_meta_override.html" => (
                "302 Found",
                "text/plain; charset=utf-8",
                b"redirecting".to_vec(),
              ),
              "/doc_redirect_policy_img_meta_override.html" => (
                "302 Found",
                "text/plain; charset=utf-8",
                b"redirecting".to_vec(),
              ),
              "/doc_redirect_policy_iframe_meta_override.html" => (
                "302 Found",
                "text/plain; charset=utf-8",
                b"redirecting".to_vec(),
              ),
              "/style_redirect.css" => (
                "302 Found",
                "text/plain; charset=utf-8",
                b"redirecting".to_vec(),
              ),
              "/style_redirect_policy.css" => (
                "302 Found",
                "text/plain; charset=utf-8",
                b"redirecting".to_vec(),
              ),
              "/style_import_nested_policy.css" => (
                "200 OK",
                "text/css; charset=utf-8",
                br#"@import url("import_policy.css");
body { }"#
                  .to_vec(),
              ),
              "/style_import_policy.css" => (
                "200 OK",
                "text/css; charset=utf-8",
                br#"@import url("import.css");
@font-face { font-family: "TestFont"; src: url("font.woff2"); }
body { font-family: "TestFont"; }"#
                  .to_vec(),
              ),
              "/style_import.css" => (
                "200 OK",
                "text/css; charset=utf-8",
                br#"@import url("import.css");
@font-face { font-family: "TestFont"; src: url("font.woff2"); }
body { font-family: "TestFont"; }"#
                  .to_vec(),
              ),
              "/import_policy.css" => (
                "200 OK",
                "text/css; charset=utf-8",
                br#"@import url("grand.css");
@font-face { font-family: "TestFont"; src: url("font.woff2"); }
body { font-family: "TestFont"; }"#
                  .to_vec(),
              ),
              "/import.css" => (
                "200 OK",
                "text/css; charset=utf-8",
                b"body { color: red; }".to_vec(),
              ),
              "/grand.css" => (
                "200 OK",
                "text/css; charset=utf-8",
                b"body { background: rgb(0, 0, 0); }".to_vec(),
              ),
              "/font.woff2" => ("200 OK", "font/woff2", test_woff2_bytes().to_vec()),
              _ => ("404 Not Found", "text/plain", b"not found".to_vec()),
            };

            let mut extra_headers = String::new();
            if path == "/img.png" {
              if let Some(origin) = header_value(&req, "origin") {
                extra_headers.push_str(&format!("Access-Control-Allow-Origin: {origin}\r\n"));
              } else {
                extra_headers.push_str("Access-Control-Allow-Origin: *\r\n");
              }
            }
            if matches!(
              path.as_str(),
              "/doc_response_policy.html"
                | "/doc_response_policy_link_override.html"
                | "/doc_response_policy_meta_override.html"
                | "/doc_response_policy_img.html"
                | "/doc_response_policy_img_meta_override.html"
                | "/doc_response_policy_img_attr_override.html"
                | "/doc_response_policy_iframe.html"
                | "/doc_response_policy_iframe_meta_override.html"
                | "/doc_response_policy_iframe_attr_override.html"
            ) {
              extra_headers.push_str("Referrer-Policy: no-referrer\r\n");
            }
            if path == "/doc_redirect_policy.html" {
              extra_headers.push_str("Location: /doc_links_style_import.html\r\n");
              extra_headers.push_str("Referrer-Policy: no-referrer\r\n");
            }
            if path == "/doc_redirect_policy_img.html" {
              extra_headers.push_str("Location: /doc_links_img.html\r\n");
              extra_headers.push_str("Referrer-Policy: no-referrer\r\n");
            }
            if path == "/doc_redirect_policy_iframe.html" {
              extra_headers.push_str("Location: /doc_links_iframe.html\r\n");
              extra_headers.push_str("Referrer-Policy: no-referrer\r\n");
            }
            if path == "/doc_redirect_policy_link_override.html" {
              extra_headers.push_str("Location: /doc_links_style_import_link_override.html\r\n");
              extra_headers.push_str("Referrer-Policy: no-referrer\r\n");
            }
            if path == "/doc_redirect_policy_meta_override.html" {
              extra_headers.push_str("Location: /doc_links_style_import_meta_origin.html\r\n");
              extra_headers.push_str("Referrer-Policy: no-referrer\r\n");
            }
            if path == "/doc_redirect_policy_img_meta_override.html" {
              extra_headers.push_str("Location: /doc_links_img_meta_origin.html\r\n");
              extra_headers.push_str("Referrer-Policy: no-referrer\r\n");
            }
            if path == "/doc_redirect_policy_iframe_meta_override.html" {
              extra_headers.push_str("Location: /doc_links_iframe_meta_origin.html\r\n");
              extra_headers.push_str("Referrer-Policy: no-referrer\r\n");
            }
            if matches!(path.as_str(), "/style_redirect.css" | "/style_redirect_policy.css") {
              extra_headers.push_str("Location: /style_import.css\r\n");
            }
            if path == "/style_redirect_policy.css" {
              extra_headers.push_str("Referrer-Policy: no-referrer\r\n");
            }
            if matches!(path.as_str(), "/style_import_policy.css" | "/import_policy.css") {
              extra_headers.push_str("Referrer-Policy: no-referrer\r\n");
            }

            let response = format!(
              "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\n{extra_headers}Content-Length: {}\r\nConnection: close\r\n\r\n",
              body.len()
            );
            let _ = stream.write_all(response.as_bytes());
            let _ = stream.write_all(&body);
          }
          Err(err) if err.kind() == io::ErrorKind::WouldBlock => {
            thread::sleep(Duration::from_millis(5));
          }
          Err(_) => break,
        }
      }
    });

    Some(Self {
      base_url,
      requests,
      shutdown,
      join: Some(join),
    })
  }

  fn wait_for_request(&self, predicate: impl Fn(&CapturedRequest) -> bool, context: &str) {
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
      if self
        .requests
        .lock()
        .map(|guard| guard.iter().any(|req| predicate(req)))
        .unwrap_or(false)
      {
        break;
      }
      thread::sleep(Duration::from_millis(5));
    }
    thread::sleep(Duration::from_millis(10));
    let matches = self
      .requests
      .lock()
      .map(|guard| guard.iter().filter(|req| predicate(req)).count())
      .unwrap_or(0);
    assert!(
      matches > 0,
      "{context}\n\nCaptured requests:\n{}",
      self
        .take_requests()
        .iter()
        .map(|req| req.headers.clone())
        .collect::<Vec<_>>()
        .join("\n---\n")
    );
  }

  fn take_requests(&self) -> Vec<CapturedRequest> {
    self
      .requests
      .lock()
      .unwrap_or_else(|e| e.into_inner())
      .clone()
  }
}

impl Drop for HeaderCaptureServer {
  fn drop(&mut self) {
    self.shutdown.store(true, Ordering::SeqCst);
    if let Some(join) = self.join.take() {
      let _ = join.join();
    }
  }
}

fn minimal_png() -> &'static [u8] {
  // 1x1 transparent PNG.
  // Generated once to avoid needing a PNG encoder in tests.
  &[
    0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48,
    0x44, 0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00,
    0x00, 0x1F, 0x15, 0xC4, 0x89, 0x00, 0x00, 0x00, 0x0A, 0x49, 0x44, 0x41, 0x54, 0x78,
    0x9C, 0x63, 0x00, 0x01, 0x00, 0x00, 0x05, 0x00, 0x01, 0x0D, 0x0A, 0x2D, 0xB4, 0x00,
    0x00, 0x00, 0x00, 0x49, 0x45, 0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82,
  ]
}

fn build_renderer() -> FastRender {
  let mut toggles = HashMap::new();
  toggles.insert("FASTR_FETCH_LINK_CSS".to_string(), "1".to_string());
  let toggles = Arc::new(RuntimeToggles::from_map(toggles));
  let mut config = FastRenderConfig::new();
  config.runtime_toggles = toggles;
  FastRender::with_config_and_fetcher(config, Some(Arc::new(HttpFetcher::new())))
    .expect("renderer")
}

#[test]
fn img_referrerpolicy_no_referrer_suppresses_referer_header() {
  let Some(server) =
    HeaderCaptureServer::start("img_referrerpolicy_no_referrer_suppresses_referer_header")
  else {
    return;
  };

  let html = format!(
    r#"<img src="{}/img.png" referrerpolicy="no-referrer" style="width: 10px; height: 10px">"#,
    server.base_url
  );

  let mut renderer = build_renderer();
  let _ = renderer
    .render_html_with_stylesheets(&html, "http://doc.test/page.html", RenderOptions::new().with_viewport(32, 32))
    .expect("render");

  server.wait_for_request(
    |req| req.path == "/img.png",
    "expected image request to be issued for the test fixture",
  );
  let requests = server.take_requests();
  let img_req = requests
    .iter()
    .find(|req| req.path == "/img.png")
    .expect("expected /img.png request");
  assert!(
    header_value(&img_req.headers, "referer").is_none(),
    "expected Referer header to be omitted; got:\n{}",
    img_req.headers
  );
}

#[test]
fn img_crossorigin_no_referrer_still_sends_origin_header() {
  let Some(server) = HeaderCaptureServer::start("img_crossorigin_no_referrer_still_sends_origin_header") else {
    return;
  };

  let html = format!(
    r#"<img src="{}/img.png" crossorigin="anonymous" referrerpolicy="no-referrer" style="width: 10px; height: 10px">"#,
    server.base_url
  );

  let mut renderer = build_renderer();
  let _ = renderer
    .render_html_with_stylesheets(&html, "http://doc.test/page.html", RenderOptions::new().with_viewport(32, 32))
    .expect("render");

  server.wait_for_request(
    |req| req.path == "/img.png",
    "expected image request to be issued for the test fixture",
  );
  let requests = server.take_requests();
  let img_req = requests
    .iter()
    .find(|req| req.path == "/img.png")
    .expect("expected /img.png request");
  assert!(
    header_value(&img_req.headers, "referer").is_none(),
    "expected Referer header to be omitted; got:\n{}",
    img_req.headers
  );
  assert_eq!(
    header_value(&img_req.headers, "origin").as_deref(),
    Some("http://doc.test"),
    "expected CORS-mode image fetch to include Origin header derived from document origin"
  );
}

#[test]
fn iframe_referrerpolicy_no_referrer_suppresses_referer_header() {
  let Some(server) =
    HeaderCaptureServer::start("iframe_referrerpolicy_no_referrer_suppresses_referer_header")
  else {
    return;
  };

  let html = format!(
    r#"<iframe src="{}/frame.html" referrerpolicy="no-referrer" style="width: 10px; height: 10px; border: 0"></iframe>"#,
    server.base_url
  );

  let mut renderer = build_renderer();
  let _ = renderer
    .render_html_with_stylesheets(&html, "http://doc.test/page.html", RenderOptions::new().with_viewport(32, 32))
    .expect("render");

  server.wait_for_request(
    |req| req.path == "/frame.html",
    "expected iframe document request to be issued for the test fixture",
  );
  let requests = server.take_requests();
  let iframe_req = requests
    .iter()
    .find(|req| req.path == "/frame.html")
    .expect("expected /frame.html request");
  assert!(
    header_value(&iframe_req.headers, "referer").is_none(),
    "expected Referer header to be omitted; got:\n{}",
    iframe_req.headers
  );
}

#[test]
fn stylesheet_referrerpolicy_no_referrer_suppresses_referer_header() {
  let Some(server) =
    HeaderCaptureServer::start("stylesheet_referrerpolicy_no_referrer_suppresses_referer_header")
  else {
    return;
  };

  let html = format!(
    r#"
      <link rel="stylesheet" href="{}/style.css" referrerpolicy="no-referrer">
      <div>styled</div>
    "#,
    server.base_url
  );

  let mut renderer = build_renderer();
  let _ = renderer
    .render_html_with_stylesheets(&html, "http://doc.test/page.html", RenderOptions::new().with_viewport(32, 32))
    .expect("render");

  server.wait_for_request(
    |req| req.path == "/style.css",
    "expected stylesheet request to be issued for the test fixture",
  );
  let requests = server.take_requests();
  let css_req = requests
    .iter()
    .find(|req| req.path == "/style.css")
    .expect("expected /style.css request");
  assert!(
    header_value(&css_req.headers, "referer").is_none(),
    "expected Referer header to be omitted; got:\n{}",
    css_req.headers
  );
}

#[test]
fn stylesheet_referrerpolicy_no_referrer_suppresses_referer_for_imports_and_fonts() {
  let Some(server) = HeaderCaptureServer::start(
    "stylesheet_referrerpolicy_no_referrer_suppresses_referer_for_imports_and_fonts",
  ) else {
    return;
  };

  let html = format!(
    r#"
      <link rel="stylesheet" href="{}/style_import.css" referrerpolicy="no-referrer">
      <div>hello</div>
    "#,
    server.base_url
  );

  let mut renderer = build_renderer();
  let _ = renderer
    .render_html_with_stylesheets(
      &html,
      "http://doc.test/page.html",
      RenderOptions::new().with_viewport(32, 32),
    )
    .expect("render");

  for path in ["/style_import.css", "/import.css", "/font.woff2"] {
    server.wait_for_request(
      |req| req.path == path,
      &format!("expected {path} request to be issued for the test fixture"),
    );
  }

  let requests = server.take_requests();
  for path in ["/style_import.css", "/import.css", "/font.woff2"] {
    let req = requests
      .iter()
      .find(|req| req.path == path)
      .unwrap_or_else(|| panic!("expected {path} request"));
    assert!(
      header_value(&req.headers, "referer").is_none(),
      "expected Referer header to be omitted for {path}; got:\n{}",
      req.headers
    );
  }
}

#[test]
fn stylesheet_referrerpolicy_origin_applies_to_imports_and_fonts() {
  let Some(server) =
    HeaderCaptureServer::start("stylesheet_referrerpolicy_origin_applies_to_imports_and_fonts")
  else {
    return;
  };

  let html = format!(
    r#"
      <link rel="stylesheet" href="{}/style_import.css" referrerpolicy="origin">
      <div>hello</div>
    "#,
    server.base_url
  );
  let document_url = format!("{}/page.html", server.base_url);

  let mut renderer = build_renderer();
  let _ = renderer
    .render_html_with_stylesheets(
      &html,
      &document_url,
      RenderOptions::new().with_viewport(32, 32),
    )
    .expect("render");

  for path in ["/style_import.css", "/import.css", "/font.woff2"] {
    server.wait_for_request(
      |req| req.path == path,
      &format!("expected {path} request to be issued for the test fixture"),
    );
  }

  let expected_referer = format!("{}/", server.base_url);
  let requests = server.take_requests();
  for path in ["/style_import.css", "/import.css", "/font.woff2"] {
    let req = requests
      .iter()
      .find(|req| req.path == path)
      .unwrap_or_else(|| panic!("expected {path} request"));
    assert_eq!(
      header_value(&req.headers, "referer").as_deref(),
      Some(expected_referer.as_str()),
      "expected Referer header to be origin-only for {path}; got:\n{}",
      req.headers
    );
  }
}

#[test]
fn stylesheet_referrerpolicy_same_origin_uses_document_referrer_for_sheet_and_stylesheet_referrer_for_nested(
) {
  let Some(server) = HeaderCaptureServer::start(
    "stylesheet_referrerpolicy_same_origin_uses_document_referrer_for_sheet_and_stylesheet_referrer_for_nested",
  ) else {
    return;
  };

  let html = format!(
    r#"
      <link rel="stylesheet" href="{}/style_import.css" referrerpolicy="same-origin">
      <div>hello</div>
    "#,
    server.base_url
  );
  let document_url = format!("{}/page.html", server.base_url);

  let mut renderer = build_renderer();
  let _ = renderer
    .render_html_with_stylesheets(
      &html,
      &document_url,
      RenderOptions::new().with_viewport(32, 32),
    )
    .expect("render");

  for path in ["/style_import.css", "/import.css", "/font.woff2"] {
    server.wait_for_request(
      |req| req.path == path,
      &format!("expected {path} request to be issued for the test fixture"),
    );
  }

  let stylesheet_url = format!("{}/style_import.css", server.base_url);
  let requests = server.take_requests();
  let sheet_req = requests
    .iter()
    .find(|req| req.path == "/style_import.css")
    .expect("expected /style_import.css request");
  assert_eq!(
    header_value(&sheet_req.headers, "referer").as_deref(),
    Some(document_url.as_str()),
    "expected stylesheet request Referer to be the full document URL; got:\n{}",
    sheet_req.headers
  );

  for path in ["/import.css", "/font.woff2"] {
    let req = requests
      .iter()
      .find(|req| req.path == path)
      .unwrap_or_else(|| panic!("expected {path} request"));
    assert_eq!(
      header_value(&req.headers, "referer").as_deref(),
      Some(stylesheet_url.as_str()),
      "expected nested request Referer to be the importing stylesheet URL for {path}; got:\n{}",
      req.headers
    );
  }
}

#[test]
fn stylesheet_referrerpolicy_same_origin_omits_cross_origin_document_referrer_but_uses_stylesheet_referrer_for_nested(
) {
  let Some(server) = HeaderCaptureServer::start(
    "stylesheet_referrerpolicy_same_origin_omits_cross_origin_document_referrer_but_uses_stylesheet_referrer_for_nested",
  ) else {
    return;
  };

  let html = format!(
    r#"
      <link rel="stylesheet" href="{}/style_import.css" referrerpolicy="same-origin">
      <div>hello</div>
    "#,
    server.base_url
  );
  let document_url = "http://doc.test/page.html";

  let mut renderer = build_renderer();
  let _ = renderer
    .render_html_with_stylesheets(
      &html,
      document_url,
      RenderOptions::new().with_viewport(32, 32),
    )
    .expect("render");

  for path in ["/style_import.css", "/import.css", "/font.woff2"] {
    server.wait_for_request(
      |req| req.path == path,
      &format!("expected {path} request to be issued for the test fixture"),
    );
  }

  let requests = server.take_requests();
  let sheet_req = requests
    .iter()
    .find(|req| req.path == "/style_import.css")
    .expect("expected /style_import.css request");
  assert!(
    header_value(&sheet_req.headers, "referer").is_none(),
    "expected cross-origin document referrer to be omitted for /style_import.css; got:\n{}",
    sheet_req.headers
  );

  let stylesheet_url = format!("{}/style_import.css", server.base_url);
  for path in ["/import.css", "/font.woff2"] {
    let req = requests
      .iter()
      .find(|req| req.path == path)
      .unwrap_or_else(|| panic!("expected {path} request"));
    assert_eq!(
      header_value(&req.headers, "referer").as_deref(),
      Some(stylesheet_url.as_str()),
      "expected nested request Referer to be the importing stylesheet URL for {path}; got:\n{}",
      req.headers
    );
  }
}

#[test]
fn stylesheet_referrerpolicy_strict_origin_when_cross_origin_downgrade_omits_referer_for_sheet_but_not_nested(
) {
  let Some(server) = HeaderCaptureServer::start(
    "stylesheet_referrerpolicy_strict_origin_when_cross_origin_downgrade_omits_referer_for_sheet_but_not_nested",
  ) else {
    return;
  };

  let html = format!(
    r#"
      <link rel="stylesheet" href="{}/style_import.css" referrerpolicy="strict-origin-when-cross-origin">
      <div>hello</div>
    "#,
    server.base_url
  );
  // HTTPS document -> HTTP stylesheet is a downgrade, so strict policies must omit `Referer` for
  // the stylesheet request even though the document URL is known.
  let document_url = "https://doc.test/page.html";

  let mut renderer = build_renderer();
  let _ = renderer
    .render_html_with_stylesheets(
      &html,
      document_url,
      RenderOptions::new().with_viewport(32, 32),
    )
    .expect("render");

  for path in ["/style_import.css", "/import.css", "/font.woff2"] {
    server.wait_for_request(
      |req| req.path == path,
      &format!("expected {path} request to be issued for the test fixture"),
    );
  }

  let requests = server.take_requests();
  let sheet_req = requests
    .iter()
    .find(|req| req.path == "/style_import.css")
    .expect("expected /style_import.css request");
  assert!(
    header_value(&sheet_req.headers, "referer").is_none(),
    "expected downgraded strict policy to omit Referer for /style_import.css; got:\n{}",
    sheet_req.headers
  );

  // Nested requests should use the importing stylesheet URL as the referrer, and because those
  // requests are not a downgrade (HTTP -> HTTP), the strict policy allows the full URL through.
  let stylesheet_url = format!("{}/style_import.css", server.base_url);
  for path in ["/import.css", "/font.woff2"] {
    let req = requests
      .iter()
      .find(|req| req.path == path)
      .unwrap_or_else(|| panic!("expected {path} request"));
    assert_eq!(
      header_value(&req.headers, "referer").as_deref(),
      Some(stylesheet_url.as_str()),
      "expected nested request Referer to be the importing stylesheet URL for {path}; got:\n{}",
      req.headers
    );
  }
}

#[test]
fn stylesheet_response_referrer_policy_no_referrer_suppresses_referer_for_imports_and_fonts() {
  let Some(server) = HeaderCaptureServer::start(
    "stylesheet_response_referrer_policy_no_referrer_suppresses_referer_for_imports_and_fonts",
  ) else {
    return;
  };

  let html = format!(
    r#"
      <link rel="stylesheet" href="{}/style_import_policy.css">
      <div>hello</div>
    "#,
    server.base_url
  );

  let mut renderer = build_renderer();
  let _ = renderer
    .render_html_with_stylesheets(
      &html,
      "http://doc.test/page.html",
      RenderOptions::new().with_viewport(32, 32),
    )
    .expect("render");

  for path in ["/style_import_policy.css", "/import.css", "/font.woff2"] {
    server.wait_for_request(
      |req| req.path == path,
      &format!("expected {path} request to be issued for the test fixture"),
    );
  }

  let requests = server.take_requests();
  let stylesheet_req = requests
    .iter()
    .find(|req| req.path == "/style_import_policy.css")
    .expect("expected /style_import_policy.css request");
  assert_eq!(
    header_value(&stylesheet_req.headers, "referer").as_deref(),
    Some("http://doc.test/"),
    "expected stylesheet request Referer to be document origin; got:\n{}",
    stylesheet_req.headers
  );

  for path in ["/import.css", "/font.woff2"] {
    let req = requests
      .iter()
      .find(|req| req.path == path)
      .unwrap_or_else(|| panic!("expected {path} request"));
    assert!(
      header_value(&req.headers, "referer").is_none(),
      "expected Referer header to be omitted for {path}; got:\n{}",
      req.headers
    );
  }
}

#[test]
fn stylesheet_referrerpolicy_origin_when_cross_origin_uses_origin_for_sheet_and_stylesheet_referrer_for_nested(
) {
  let Some(server) = HeaderCaptureServer::start(
    "stylesheet_referrerpolicy_origin_when_cross_origin_uses_origin_for_sheet_and_stylesheet_referrer_for_nested",
  ) else {
    return;
  };

  let html = format!(
    r#"
      <link rel="stylesheet" href="{}/style_import.css" referrerpolicy="origin-when-cross-origin">
      <div>hello</div>
    "#,
    server.base_url
  );

  let mut renderer = build_renderer();
  let _ = renderer
    .render_html_with_stylesheets(
      &html,
      "http://doc.test/page.html",
      RenderOptions::new().with_viewport(32, 32),
    )
    .expect("render");

  for path in ["/style_import.css", "/import.css", "/font.woff2"] {
    server.wait_for_request(
      |req| req.path == path,
      &format!("expected {path} request to be issued for the test fixture"),
    );
  }

  let requests = server.take_requests();
  let sheet_req = requests
    .iter()
    .find(|req| req.path == "/style_import.css")
    .expect("expected /style_import.css request");
  assert_eq!(
    header_value(&sheet_req.headers, "referer").as_deref(),
    Some("http://doc.test/"),
    "expected cross-origin stylesheet request Referer to be the document origin; got:\n{}",
    sheet_req.headers
  );

  let stylesheet_url = format!("{}/style_import.css", server.base_url);
  for path in ["/import.css", "/font.woff2"] {
    let req = requests
      .iter()
      .find(|req| req.path == path)
      .unwrap_or_else(|| panic!("expected {path} request"));
    assert_eq!(
      header_value(&req.headers, "referer").as_deref(),
      Some(stylesheet_url.as_str()),
      "expected nested request Referer to be the importing stylesheet URL for {path}; got:\n{}",
      req.headers
    );
  }
}

#[test]
fn imported_stylesheet_response_referrer_policy_no_referrer_suppresses_referer_for_grandchildren() {
  let Some(server) = HeaderCaptureServer::start(
    "imported_stylesheet_response_referrer_policy_no_referrer_suppresses_referer_for_grandchildren",
  ) else {
    return;
  };

  let html = format!(
    r#"
      <link rel="stylesheet" href="{}/style_import_nested_policy.css">
      <div>hello</div>
    "#,
    server.base_url
  );

  let mut renderer = build_renderer();
  let _ = renderer
    .render_html_with_stylesheets(
      &html,
      "http://doc.test/page.html",
      RenderOptions::new().with_viewport(32, 32),
    )
    .expect("render");

  for path in [
    "/style_import_nested_policy.css",
    "/import_policy.css",
    "/grand.css",
    "/font.woff2",
  ] {
    server.wait_for_request(
      |req| req.path == path,
      &format!("expected {path} request to be issued for the test fixture"),
    );
  }

  let requests = server.take_requests();
  let expected_root_referrer = format!("{}/style_import_nested_policy.css", server.base_url);
  let imported_req = requests
    .iter()
    .find(|req| req.path == "/import_policy.css")
    .expect("expected /import_policy.css request");
  assert_eq!(
    header_value(&imported_req.headers, "referer").as_deref(),
    Some(expected_root_referrer.as_str()),
    "expected imported stylesheet request Referer to be the importing stylesheet URL; got:\n{}",
    imported_req.headers
  );

  for path in ["/grand.css", "/font.woff2"] {
    let req = requests
      .iter()
      .find(|req| req.path == path)
      .unwrap_or_else(|| panic!("expected {path} request"));
    assert!(
      header_value(&req.headers, "referer").is_none(),
      "expected Referer header to be omitted for {path} due to imported stylesheet Referrer-Policy; got:\n{}",
      req.headers
    );
  }
}

#[test]
fn stylesheet_redirect_uses_final_url_as_referrer_for_nested_requests() {
  let Some(server) =
    HeaderCaptureServer::start("stylesheet_redirect_uses_final_url_as_referrer_for_nested_requests")
  else {
    return;
  };

  let html = format!(
    r#"
      <link rel="stylesheet" href="{}/style_redirect.css">
      <div>hello</div>
    "#,
    server.base_url
  );

  let mut renderer = build_renderer();
  let _ = renderer
    .render_html_with_stylesheets(
      &html,
      "http://doc.test/page.html",
      RenderOptions::new().with_viewport(32, 32),
    )
    .expect("render");

  for path in ["/style_redirect.css", "/style_import.css", "/import.css", "/font.woff2"] {
    server.wait_for_request(
      |req| req.path == path,
      &format!("expected {path} request to be issued for the test fixture"),
    );
  }

  let requests = server.take_requests();
  let expected_referrer = format!("{}/style_import.css", server.base_url);
  for path in ["/import.css", "/font.woff2"] {
    let req = requests
      .iter()
      .find(|req| req.path == path)
      .unwrap_or_else(|| panic!("expected {path} request"));
    assert_eq!(
      header_value(&req.headers, "referer").as_deref(),
      Some(expected_referrer.as_str()),
      "expected nested request Referer to use the final stylesheet URL after redirects for {path}; got:\n{}",
      req.headers
    );
  }
}

#[test]
fn stylesheet_redirect_response_referrer_policy_no_referrer_suppresses_referer_for_followup_request(
) {
  let Some(server) = HeaderCaptureServer::start(
    "stylesheet_redirect_response_referrer_policy_no_referrer_suppresses_referer_for_followup_request",
  ) else {
    return;
  };

  let html = format!(
    r#"
      <link rel="stylesheet" href="{}/style_redirect_policy.css">
      <div>hello</div>
    "#,
    server.base_url
  );

  let mut renderer = build_renderer();
  let _ = renderer
    .render_html_with_stylesheets(
      &html,
      "http://doc.test/page.html",
      RenderOptions::new().with_viewport(32, 32),
    )
    .expect("render");

  for path in ["/style_redirect_policy.css", "/style_import.css", "/import.css", "/font.woff2"] {
    server.wait_for_request(
      |req| req.path == path,
      &format!("expected {path} request to be issued for the test fixture"),
    );
  }

  let requests = server.take_requests();
  let followup_req = requests
    .iter()
    .find(|req| req.path == "/style_import.css")
    .expect("expected redirect follow-up request to /style_import.css");
  assert!(
    header_value(&followup_req.headers, "referer").is_none(),
    "expected redirect follow-up request Referer to be omitted due to redirect Referrer-Policy: no-referrer; got:\n{}",
    followup_req.headers
  );
}

#[test]
fn stylesheet_redirect_response_referrer_policy_no_referrer_suppresses_referer_for_nested_requests()
{
  let Some(server) = HeaderCaptureServer::start(
    "stylesheet_redirect_response_referrer_policy_no_referrer_suppresses_referer_for_nested_requests",
  ) else {
    return;
  };

  let html = format!(
    r#"
      <link rel="stylesheet" href="{}/style_redirect_policy.css">
      <div>hello</div>
    "#,
    server.base_url
  );

  let mut renderer = build_renderer();
  let _ = renderer
    .render_html_with_stylesheets(
      &html,
      "http://doc.test/page.html",
      RenderOptions::new().with_viewport(32, 32),
    )
    .expect("render");

  for path in ["/style_redirect_policy.css", "/style_import.css", "/import.css", "/font.woff2"] {
    server.wait_for_request(
      |req| req.path == path,
      &format!("expected {path} request to be issued for the test fixture"),
    );
  }

  let requests = server.take_requests();
  for path in ["/style_import.css", "/import.css", "/font.woff2"] {
    let req = requests
      .iter()
      .find(|req| req.path == path)
      .unwrap_or_else(|| panic!("expected {path} request"));
    assert!(
      header_value(&req.headers, "referer").is_none(),
      "expected Referer header to be omitted for {path} due to redirect Referrer-Policy: no-referrer; got:\n{}",
      req.headers
    );
  }
}

#[test]
fn meta_referrer_policy_no_referrer_suppresses_referer_for_stylesheets_and_nested_requests() {
  let Some(server) = HeaderCaptureServer::start(
    "meta_referrer_policy_no_referrer_suppresses_referer_for_stylesheets_and_nested_requests",
  ) else {
    return;
  };

  let html = format!(
    r#"<!doctype html>
      <html>
        <head>
          <meta name="referrer" content="no-referrer">
          <link rel="stylesheet" href="{}/style_import.css">
        </head>
        <body><div>hello</div></body>
      </html>"#,
    server.base_url
  );
  let document_url = format!("{}/page.html", server.base_url);

  let mut renderer = build_renderer();
  let _ = renderer
    .render_html_with_stylesheets(
      &html,
      &document_url,
      RenderOptions::new().with_viewport(32, 32),
    )
    .expect("render");

  for path in ["/style_import.css", "/import.css", "/font.woff2"] {
    server.wait_for_request(
      |req| req.path == path,
      &format!("expected {path} request to be issued for the test fixture"),
    );
  }

  let requests = server.take_requests();
  for path in ["/style_import.css", "/import.css", "/font.woff2"] {
    let req = requests
      .iter()
      .find(|req| req.path == path)
      .unwrap_or_else(|| panic!("expected {path} request"));
    assert!(
      header_value(&req.headers, "referer").is_none(),
      "expected Referer header to be omitted for {path} due to meta Referrer-Policy; got:\n{}",
      req.headers
    );
  }
}

#[test]
fn meta_referrer_policy_no_referrer_suppresses_referer_for_images() {
  let Some(server) = HeaderCaptureServer::start(
    "meta_referrer_policy_no_referrer_suppresses_referer_for_images",
  ) else {
    return;
  };

  let html = format!(
    r#"<!doctype html>
      <html>
        <head>
          <meta name="referrer" content="no-referrer">
        </head>
        <body>
          <img src="{}/img.png" style="width: 10px; height: 10px">
        </body>
      </html>"#,
    server.base_url
  );

  let mut renderer = build_renderer();
  let _ = renderer
    .render_html_with_stylesheets(
      &html,
      "http://doc.test/page.html",
      RenderOptions::new().with_viewport(32, 32),
    )
    .expect("render");

  server.wait_for_request(
    |req| req.path == "/img.png",
    "expected image request to be issued for the test fixture",
  );

  let requests = server.take_requests();
  let img_req = requests
    .iter()
    .find(|req| req.path == "/img.png")
    .expect("expected /img.png request");
  assert!(
    header_value(&img_req.headers, "referer").is_none(),
    "expected meta Referrer-Policy to suppress Referer for /img.png; got:\n{}",
    img_req.headers
  );
}

#[test]
fn meta_referrer_policy_no_referrer_suppresses_referer_for_iframes() {
  let Some(server) = HeaderCaptureServer::start(
    "meta_referrer_policy_no_referrer_suppresses_referer_for_iframes",
  ) else {
    return;
  };

  let html = format!(
    r#"<!doctype html>
      <html>
        <head>
          <meta name="referrer" content="no-referrer">
        </head>
        <body>
          <iframe src="{}/frame.html" style="width: 10px; height: 10px"></iframe>
        </body>
      </html>"#,
    server.base_url
  );

  let mut renderer = build_renderer();
  let _ = renderer
    .render_html_with_stylesheets(
      &html,
      "http://doc.test/page.html",
      RenderOptions::new().with_viewport(32, 32),
    )
    .expect("render");

  server.wait_for_request(
    |req| req.path == "/frame.html",
    "expected iframe request to be issued for the test fixture",
  );

  let requests = server.take_requests();
  let frame_req = requests
    .iter()
    .find(|req| req.path == "/frame.html")
    .expect("expected /frame.html request");
  assert!(
    header_value(&frame_req.headers, "referer").is_none(),
    "expected meta Referrer-Policy to suppress Referer for /frame.html; got:\n{}",
    frame_req.headers
  );
}

#[test]
fn meta_referrer_policy_no_referrer_allows_referrerpolicy_override_for_images() {
  let Some(server) = HeaderCaptureServer::start(
    "meta_referrer_policy_no_referrer_allows_referrerpolicy_override_for_images",
  ) else {
    return;
  };

  let html = r#"<!doctype html>
    <html>
      <head>
        <meta name="referrer" content="no-referrer">
      </head>
      <body>
        <img src="/img.png" referrerpolicy="origin" style="width: 10px; height: 10px">
      </body>
    </html>"#;
  let document_url = format!("{}/page.html", server.base_url);

  let mut renderer = build_renderer();
  let _ = renderer
    .render_html_with_stylesheets(&html, &document_url, RenderOptions::new().with_viewport(32, 32))
    .expect("render");

  server.wait_for_request(
    |req| req.path == "/img.png",
    "expected image request to be issued for the test fixture",
  );

  let expected_referer = format!("{}/", server.base_url);
  let requests = server.take_requests();
  let img_req = requests
    .iter()
    .find(|req| req.path == "/img.png")
    .expect("expected /img.png request");
  assert_eq!(
    header_value(&img_req.headers, "referer").as_deref(),
    Some(expected_referer.as_str()),
    "expected img referrerpolicy override to win over meta Referrer-Policy for /img.png; got:\n{}",
    img_req.headers
  );
}

#[test]
fn meta_referrer_policy_no_referrer_allows_referrerpolicy_override_for_iframes() {
  let Some(server) = HeaderCaptureServer::start(
    "meta_referrer_policy_no_referrer_allows_referrerpolicy_override_for_iframes",
  ) else {
    return;
  };

  let html = r#"<!doctype html>
    <html>
      <head>
        <meta name="referrer" content="no-referrer">
      </head>
      <body>
        <iframe src="/frame.html" referrerpolicy="origin" style="width: 10px; height: 10px"></iframe>
      </body>
    </html>"#;
  let document_url = format!("{}/page.html", server.base_url);

  let mut renderer = build_renderer();
  let _ = renderer
    .render_html_with_stylesheets(&html, &document_url, RenderOptions::new().with_viewport(32, 32))
    .expect("render");

  server.wait_for_request(
    |req| req.path == "/frame.html",
    "expected iframe request to be issued for the test fixture",
  );

  let expected_referer = format!("{}/", server.base_url);
  let requests = server.take_requests();
  let frame_req = requests
    .iter()
    .find(|req| req.path == "/frame.html")
    .expect("expected /frame.html request");
  assert_eq!(
    header_value(&frame_req.headers, "referer").as_deref(),
    Some(expected_referer.as_str()),
    "expected iframe referrerpolicy override to win over meta Referrer-Policy for /frame.html; got:\n{}",
    frame_req.headers
  );
}

#[test]
fn meta_referrer_policy_no_referrer_allows_link_referrerpolicy_override_for_nested_requests() {
  let Some(server) = HeaderCaptureServer::start(
    "meta_referrer_policy_no_referrer_allows_link_referrerpolicy_override_for_nested_requests",
  ) else {
    return;
  };

  let html = format!(
    r#"<!doctype html>
      <html>
        <head>
          <meta name="referrer" content="no-referrer">
          <link rel="stylesheet" href="{}/style_import.css" referrerpolicy="origin">
        </head>
        <body><div>hello</div></body>
      </html>"#,
    server.base_url
  );
  let document_url = format!("{}/page.html", server.base_url);

  let mut renderer = build_renderer();
  let _ = renderer
    .render_html_with_stylesheets(
      &html,
      &document_url,
      RenderOptions::new().with_viewport(32, 32),
    )
    .expect("render");

  for path in ["/style_import.css", "/import.css", "/font.woff2"] {
    server.wait_for_request(
      |req| req.path == path,
      &format!("expected {path} request to be issued for the test fixture"),
    );
  }

  let expected_referer = format!("{}/", server.base_url);
  let requests = server.take_requests();
  for path in ["/style_import.css", "/import.css", "/font.woff2"] {
    let req = requests
      .iter()
      .find(|req| req.path == path)
      .unwrap_or_else(|| panic!("expected {path} request"));
    assert_eq!(
      header_value(&req.headers, "referer").as_deref(),
      Some(expected_referer.as_str()),
      "expected link referrerpolicy override to win over meta Referrer-Policy for {path}; got:\n{}",
      req.headers
    );
  }
}

#[test]
fn fetched_document_meta_referrer_policy_no_referrer_suppresses_referer_for_stylesheets_and_nested_requests(
) {
  let Some(server) = HeaderCaptureServer::start(
    "fetched_document_meta_referrer_policy_no_referrer_suppresses_referer_for_stylesheets_and_nested_requests",
  ) else {
    return;
  };

  let doc_url = format!("{}/doc_meta_policy.html", server.base_url);

  let mut renderer = build_renderer();
  let _ = renderer
    .render_url_with_options(&doc_url, RenderOptions::new().with_viewport(32, 32))
    .expect("render");

  for path in [
    "/doc_meta_policy.html",
    "/style_import.css",
    "/import.css",
    "/font.woff2",
  ] {
    server.wait_for_request(
      |req| req.path == path,
      &format!("expected {path} request to be issued for the test fixture"),
    );
  }

  let requests = server.take_requests();
  for path in ["/style_import.css", "/import.css", "/font.woff2"] {
    let req = requests
      .iter()
      .find(|req| req.path == path)
      .unwrap_or_else(|| panic!("expected {path} request"));
    assert!(
      header_value(&req.headers, "referer").is_none(),
      "expected meta Referrer-Policy to suppress Referer for {path}; got:\n{}",
      req.headers
    );
  }
}

#[test]
fn document_response_referrer_policy_no_referrer_suppresses_referer_for_stylesheets_and_nested_requests(
) {
  let Some(server) = HeaderCaptureServer::start(
    "document_response_referrer_policy_no_referrer_suppresses_referer_for_stylesheets_and_nested_requests",
  ) else {
    return;
  };

  let doc_url = format!("{}/doc_response_policy.html", server.base_url);

  let mut renderer = build_renderer();
  let _ = renderer
    .render_url_with_options(&doc_url, RenderOptions::new().with_viewport(32, 32))
    .expect("render");

  for path in [
    "/doc_response_policy.html",
    "/style_import.css",
    "/import.css",
    "/font.woff2",
  ] {
    server.wait_for_request(
      |req| req.path == path,
      &format!("expected {path} request to be issued for the test fixture"),
    );
  }

  let requests = server.take_requests();
  for path in ["/style_import.css", "/import.css", "/font.woff2"] {
    let req = requests
      .iter()
      .find(|req| req.path == path)
      .unwrap_or_else(|| panic!("expected {path} request"));
    assert!(
      header_value(&req.headers, "referer").is_none(),
      "expected Referer header to be omitted for {path} due to document response Referrer-Policy; got:\n{}",
      req.headers
    );
  }
}

#[test]
fn document_response_referrer_policy_no_referrer_suppresses_referer_for_images() {
  let Some(server) = HeaderCaptureServer::start(
    "document_response_referrer_policy_no_referrer_suppresses_referer_for_images",
  ) else {
    return;
  };

  let doc_url = format!("{}/doc_response_policy_img.html", server.base_url);

  let mut renderer = build_renderer();
  let _ = renderer
    .render_url_with_options(&doc_url, RenderOptions::new().with_viewport(32, 32))
    .expect("render");

  for path in ["/doc_response_policy_img.html", "/img.png"] {
    server.wait_for_request(
      |req| req.path == path,
      &format!("expected {path} request to be issued for the test fixture"),
    );
  }

  let requests = server.take_requests();
  let img_req = requests
    .iter()
    .find(|req| req.path == "/img.png")
    .expect("expected /img.png request");
  assert!(
    header_value(&img_req.headers, "referer").is_none(),
    "expected document response Referrer-Policy to suppress Referer for /img.png; got:\n{}",
    img_req.headers
  );
}

#[test]
fn document_response_referrer_policy_no_referrer_suppresses_referer_for_iframes() {
  let Some(server) = HeaderCaptureServer::start(
    "document_response_referrer_policy_no_referrer_suppresses_referer_for_iframes",
  ) else {
    return;
  };

  let doc_url = format!("{}/doc_response_policy_iframe.html", server.base_url);

  let mut renderer = build_renderer();
  let _ = renderer
    .render_url_with_options(&doc_url, RenderOptions::new().with_viewport(32, 32))
    .expect("render");

  for path in ["/doc_response_policy_iframe.html", "/frame.html"] {
    server.wait_for_request(
      |req| req.path == path,
      &format!("expected {path} request to be issued for the test fixture"),
    );
  }

  let requests = server.take_requests();
  let frame_req = requests
    .iter()
    .find(|req| req.path == "/frame.html")
    .expect("expected /frame.html request");
  assert!(
    header_value(&frame_req.headers, "referer").is_none(),
    "expected document response Referrer-Policy to suppress Referer for /frame.html; got:\n{}",
    frame_req.headers
  );
}

#[test]
fn document_response_referrer_policy_no_referrer_allows_meta_override_for_nested_requests() {
  let Some(server) = HeaderCaptureServer::start(
    "document_response_referrer_policy_no_referrer_allows_meta_override_for_nested_requests",
  ) else {
    return;
  };

  let doc_url = format!("{}/doc_response_policy_meta_override.html", server.base_url);

  let mut renderer = build_renderer();
  let _ = renderer
    .render_url_with_options(&doc_url, RenderOptions::new().with_viewport(32, 32))
    .expect("render");

  for path in [
    "/doc_response_policy_meta_override.html",
    "/style_import.css",
    "/import.css",
    "/font.woff2",
  ] {
    server.wait_for_request(
      |req| req.path == path,
      &format!("expected {path} request to be issued for the test fixture"),
    );
  }

  let expected_referer = format!("{}/", server.base_url);
  let requests = server.take_requests();
  for path in ["/style_import.css", "/import.css", "/font.woff2"] {
    let req = requests
      .iter()
      .find(|req| req.path == path)
      .unwrap_or_else(|| panic!("expected {path} request"));
    assert_eq!(
      header_value(&req.headers, "referer").as_deref(),
      Some(expected_referer.as_str()),
      "expected meta Referrer-Policy override to win over document response Referrer-Policy for {path}; got:\n{}",
      req.headers
    );
  }
}

#[test]
fn document_response_referrer_policy_no_referrer_allows_meta_override_for_images() {
  let Some(server) = HeaderCaptureServer::start(
    "document_response_referrer_policy_no_referrer_allows_meta_override_for_images",
  ) else {
    return;
  };

  let doc_url = format!("{}/doc_response_policy_img_meta_override.html", server.base_url);

  let mut renderer = build_renderer();
  let _ = renderer
    .render_url_with_options(&doc_url, RenderOptions::new().with_viewport(32, 32))
    .expect("render");

  for path in ["/doc_response_policy_img_meta_override.html", "/img.png"] {
    server.wait_for_request(
      |req| req.path == path,
      &format!("expected {path} request to be issued for the test fixture"),
    );
  }

  let expected_referer = format!("{}/", server.base_url);
  let requests = server.take_requests();
  let img_req = requests
    .iter()
    .find(|req| req.path == "/img.png")
    .expect("expected /img.png request");
  assert_eq!(
    header_value(&img_req.headers, "referer").as_deref(),
    Some(expected_referer.as_str()),
    "expected meta Referrer-Policy override to win over document response Referrer-Policy for /img.png; got:\n{}",
    img_req.headers
  );
}

#[test]
fn document_response_referrer_policy_no_referrer_allows_meta_override_for_iframes() {
  let Some(server) = HeaderCaptureServer::start(
    "document_response_referrer_policy_no_referrer_allows_meta_override_for_iframes",
  ) else {
    return;
  };

  let doc_url = format!(
    "{}/doc_response_policy_iframe_meta_override.html",
    server.base_url
  );

  let mut renderer = build_renderer();
  let _ = renderer
    .render_url_with_options(&doc_url, RenderOptions::new().with_viewport(32, 32))
    .expect("render");

  for path in ["/doc_response_policy_iframe_meta_override.html", "/frame.html"] {
    server.wait_for_request(
      |req| req.path == path,
      &format!("expected {path} request to be issued for the test fixture"),
    );
  }

  let expected_referer = format!("{}/", server.base_url);
  let requests = server.take_requests();
  let frame_req = requests
    .iter()
    .find(|req| req.path == "/frame.html")
    .expect("expected /frame.html request");
  assert_eq!(
    header_value(&frame_req.headers, "referer").as_deref(),
    Some(expected_referer.as_str()),
    "expected meta Referrer-Policy override to win over document response Referrer-Policy for /frame.html; got:\n{}",
    frame_req.headers
  );
}

#[test]
fn document_response_referrer_policy_no_referrer_allows_referrerpolicy_override_for_images() {
  let Some(server) = HeaderCaptureServer::start(
    "document_response_referrer_policy_no_referrer_allows_referrerpolicy_override_for_images",
  ) else {
    return;
  };

  let doc_url = format!("{}/doc_response_policy_img_attr_override.html", server.base_url);

  let mut renderer = build_renderer();
  let _ = renderer
    .render_url_with_options(&doc_url, RenderOptions::new().with_viewport(32, 32))
    .expect("render");

  for path in ["/doc_response_policy_img_attr_override.html", "/img.png"] {
    server.wait_for_request(
      |req| req.path == path,
      &format!("expected {path} request to be issued for the test fixture"),
    );
  }

  let expected_referer = format!("{}/", server.base_url);
  let requests = server.take_requests();
  let img_req = requests
    .iter()
    .find(|req| req.path == "/img.png")
    .expect("expected /img.png request");
  assert_eq!(
    header_value(&img_req.headers, "referer").as_deref(),
    Some(expected_referer.as_str()),
    "expected img referrerpolicy override to win over document response Referrer-Policy for /img.png; got:\n{}",
    img_req.headers
  );
}

#[test]
fn document_response_referrer_policy_no_referrer_allows_referrerpolicy_override_for_iframes() {
  let Some(server) = HeaderCaptureServer::start(
    "document_response_referrer_policy_no_referrer_allows_referrerpolicy_override_for_iframes",
  ) else {
    return;
  };

  let doc_url = format!(
    "{}/doc_response_policy_iframe_attr_override.html",
    server.base_url
  );

  let mut renderer = build_renderer();
  let _ = renderer
    .render_url_with_options(&doc_url, RenderOptions::new().with_viewport(32, 32))
    .expect("render");

  for path in ["/doc_response_policy_iframe_attr_override.html", "/frame.html"] {
    server.wait_for_request(
      |req| req.path == path,
      &format!("expected {path} request to be issued for the test fixture"),
    );
  }

  let expected_referer = format!("{}/", server.base_url);
  let requests = server.take_requests();
  let frame_req = requests
    .iter()
    .find(|req| req.path == "/frame.html")
    .expect("expected /frame.html request");
  assert_eq!(
    header_value(&frame_req.headers, "referer").as_deref(),
    Some(expected_referer.as_str()),
    "expected iframe referrerpolicy override to win over document response Referrer-Policy for /frame.html; got:\n{}",
    frame_req.headers
  );
}

#[test]
fn document_response_referrer_policy_no_referrer_allows_link_referrerpolicy_override_for_nested_requests(
) {
  let Some(server) = HeaderCaptureServer::start(
    "document_response_referrer_policy_no_referrer_allows_link_referrerpolicy_override_for_nested_requests",
  ) else {
    return;
  };

  let doc_url = format!("{}/doc_response_policy_link_override.html", server.base_url);

  let mut renderer = build_renderer();
  let _ = renderer
    .render_url_with_options(&doc_url, RenderOptions::new().with_viewport(32, 32))
    .expect("render");

  for path in [
    "/doc_response_policy_link_override.html",
    "/style_import.css",
    "/import.css",
    "/font.woff2",
  ] {
    server.wait_for_request(
      |req| req.path == path,
      &format!("expected {path} request to be issued for the test fixture"),
    );
  }

  let expected_referer = format!("{}/", server.base_url);
  let requests = server.take_requests();
  for path in ["/style_import.css", "/import.css", "/font.woff2"] {
    let req = requests
      .iter()
      .find(|req| req.path == path)
      .unwrap_or_else(|| panic!("expected {path} request"));
    assert_eq!(
      header_value(&req.headers, "referer").as_deref(),
      Some(expected_referer.as_str()),
      "expected link referrerpolicy override to win over document response Referrer-Policy for {path}; got:\n{}",
      req.headers
    );
  }
}

#[test]
fn document_redirect_response_referrer_policy_no_referrer_suppresses_referer_for_stylesheets_and_nested_requests(
) {
  let Some(server) = HeaderCaptureServer::start(
    "document_redirect_response_referrer_policy_no_referrer_suppresses_referer_for_stylesheets_and_nested_requests",
  ) else {
    return;
  };

  let doc_url = format!("{}/doc_redirect_policy.html", server.base_url);

  let mut renderer = build_renderer();
  let _ = renderer
    .render_url_with_options(&doc_url, RenderOptions::new().with_viewport(32, 32))
    .expect("render");

  for path in [
    "/doc_redirect_policy.html",
    "/doc_links_style_import.html",
    "/style_import.css",
    "/import.css",
    "/font.woff2",
  ] {
    server.wait_for_request(
      |req| req.path == path,
      &format!("expected {path} request to be issued for the test fixture"),
    );
  }

  let requests = server.take_requests();
  for path in ["/style_import.css", "/import.css", "/font.woff2"] {
    let req = requests
      .iter()
      .find(|req| req.path == path)
      .unwrap_or_else(|| panic!("expected {path} request"));
    assert!(
      header_value(&req.headers, "referer").is_none(),
      "expected Referer header to be omitted for {path} due to redirect Referrer-Policy: no-referrer; got:\n{}",
      req.headers
    );
  }
}

#[test]
fn document_redirect_response_referrer_policy_no_referrer_suppresses_referer_for_images() {
  let Some(server) = HeaderCaptureServer::start(
    "document_redirect_response_referrer_policy_no_referrer_suppresses_referer_for_images",
  ) else {
    return;
  };

  let doc_url = format!("{}/doc_redirect_policy_img.html", server.base_url);

  let mut renderer = build_renderer();
  let _ = renderer
    .render_url_with_options(&doc_url, RenderOptions::new().with_viewport(32, 32))
    .expect("render");

  for path in [
    "/doc_redirect_policy_img.html",
    "/doc_links_img.html",
    "/img.png",
  ] {
    server.wait_for_request(
      |req| req.path == path,
      &format!("expected {path} request to be issued for the test fixture"),
    );
  }

  let requests = server.take_requests();
  let img_req = requests
    .iter()
    .find(|req| req.path == "/img.png")
    .expect("expected /img.png request");
  assert!(
    header_value(&img_req.headers, "referer").is_none(),
    "expected redirect Referrer-Policy to suppress Referer for /img.png; got:\n{}",
    img_req.headers
  );
}

#[test]
fn document_redirect_response_referrer_policy_no_referrer_suppresses_referer_for_iframes() {
  let Some(server) = HeaderCaptureServer::start(
    "document_redirect_response_referrer_policy_no_referrer_suppresses_referer_for_iframes",
  ) else {
    return;
  };

  let doc_url = format!("{}/doc_redirect_policy_iframe.html", server.base_url);

  let mut renderer = build_renderer();
  let _ = renderer
    .render_url_with_options(&doc_url, RenderOptions::new().with_viewport(32, 32))
    .expect("render");

  for path in [
    "/doc_redirect_policy_iframe.html",
    "/doc_links_iframe.html",
    "/frame.html",
  ] {
    server.wait_for_request(
      |req| req.path == path,
      &format!("expected {path} request to be issued for the test fixture"),
    );
  }

  let requests = server.take_requests();
  let frame_req = requests
    .iter()
    .find(|req| req.path == "/frame.html")
    .expect("expected /frame.html request");
  assert!(
    header_value(&frame_req.headers, "referer").is_none(),
    "expected redirect Referrer-Policy to suppress Referer for /frame.html; got:\n{}",
    frame_req.headers
  );
}

#[test]
fn document_redirect_response_referrer_policy_no_referrer_allows_link_referrerpolicy_override_for_stylesheets_and_nested_requests(
) {
  let Some(server) = HeaderCaptureServer::start(
    "document_redirect_response_referrer_policy_no_referrer_allows_link_referrerpolicy_override_for_stylesheets_and_nested_requests",
  ) else {
    return;
  };

  let doc_url = format!("{}/doc_redirect_policy_link_override.html", server.base_url);

  let mut renderer = build_renderer();
  let _ = renderer
    .render_url_with_options(&doc_url, RenderOptions::new().with_viewport(32, 32))
    .expect("render");

  for path in [
    "/doc_redirect_policy_link_override.html",
    "/doc_links_style_import_link_override.html",
    "/style_import.css",
    "/import.css",
    "/font.woff2",
  ] {
    server.wait_for_request(
      |req| req.path == path,
      &format!("expected {path} request to be issued for the test fixture"),
    );
  }

  let expected_referer = format!("{}/", server.base_url);
  let requests = server.take_requests();
  for path in ["/style_import.css", "/import.css", "/font.woff2"] {
    let req = requests
      .iter()
      .find(|req| req.path == path)
      .unwrap_or_else(|| panic!("expected {path} request"));
    assert_eq!(
      header_value(&req.headers, "referer").as_deref(),
      Some(expected_referer.as_str()),
      "expected link referrerpolicy override to win over redirect Referrer-Policy for {path}; got:\n{}",
      req.headers
    );
  }
}

#[test]
fn document_redirect_response_referrer_policy_no_referrer_allows_meta_override_for_stylesheets_and_nested_requests(
) {
  let Some(server) = HeaderCaptureServer::start(
    "document_redirect_response_referrer_policy_no_referrer_allows_meta_override_for_stylesheets_and_nested_requests",
  ) else {
    return;
  };

  let doc_url = format!("{}/doc_redirect_policy_meta_override.html", server.base_url);

  let mut renderer = build_renderer();
  let _ = renderer
    .render_url_with_options(&doc_url, RenderOptions::new().with_viewport(32, 32))
    .expect("render");

  for path in [
    "/doc_redirect_policy_meta_override.html",
    "/doc_links_style_import_meta_origin.html",
    "/style_import.css",
    "/import.css",
    "/font.woff2",
  ] {
    server.wait_for_request(
      |req| req.path == path,
      &format!("expected {path} request to be issued for the test fixture"),
    );
  }

  let expected_referer = format!("{}/", server.base_url);
  let requests = server.take_requests();
  for path in ["/style_import.css", "/import.css", "/font.woff2"] {
    let req = requests
      .iter()
      .find(|req| req.path == path)
      .unwrap_or_else(|| panic!("expected {path} request"));
    assert_eq!(
      header_value(&req.headers, "referer").as_deref(),
      Some(expected_referer.as_str()),
      "expected meta Referrer-Policy override to win over redirect Referrer-Policy for {path}; got:\n{}",
      req.headers
    );
  }
}

#[test]
fn document_redirect_response_referrer_policy_no_referrer_allows_meta_override_for_images() {
  let Some(server) = HeaderCaptureServer::start(
    "document_redirect_response_referrer_policy_no_referrer_allows_meta_override_for_images",
  ) else {
    return;
  };

  let doc_url = format!("{}/doc_redirect_policy_img_meta_override.html", server.base_url);

  let mut renderer = build_renderer();
  let _ = renderer
    .render_url_with_options(&doc_url, RenderOptions::new().with_viewport(32, 32))
    .expect("render");

  for path in [
    "/doc_redirect_policy_img_meta_override.html",
    "/doc_links_img_meta_origin.html",
    "/img.png",
  ] {
    server.wait_for_request(
      |req| req.path == path,
      &format!("expected {path} request to be issued for the test fixture"),
    );
  }

  let expected_referer = format!("{}/", server.base_url);
  let requests = server.take_requests();
  let img_req = requests
    .iter()
    .find(|req| req.path == "/img.png")
    .expect("expected /img.png request");
  assert_eq!(
    header_value(&img_req.headers, "referer").as_deref(),
    Some(expected_referer.as_str()),
    "expected meta Referrer-Policy override to win over redirect Referrer-Policy for /img.png; got:\n{}",
    img_req.headers
  );
}

#[test]
fn document_redirect_response_referrer_policy_no_referrer_allows_meta_override_for_iframes() {
  let Some(server) = HeaderCaptureServer::start(
    "document_redirect_response_referrer_policy_no_referrer_allows_meta_override_for_iframes",
  ) else {
    return;
  };

  let doc_url = format!(
    "{}/doc_redirect_policy_iframe_meta_override.html",
    server.base_url
  );

  let mut renderer = build_renderer();
  let _ = renderer
    .render_url_with_options(&doc_url, RenderOptions::new().with_viewport(32, 32))
    .expect("render");

  for path in [
    "/doc_redirect_policy_iframe_meta_override.html",
    "/doc_links_iframe_meta_origin.html",
    "/frame.html",
  ] {
    server.wait_for_request(
      |req| req.path == path,
      &format!("expected {path} request to be issued for the test fixture"),
    );
  }

  let expected_referer = format!("{}/", server.base_url);
  let requests = server.take_requests();
  let frame_req = requests
    .iter()
    .find(|req| req.path == "/frame.html")
    .expect("expected /frame.html request");
  assert_eq!(
    header_value(&frame_req.headers, "referer").as_deref(),
    Some(expected_referer.as_str()),
    "expected meta Referrer-Policy override to win over redirect Referrer-Policy for /frame.html; got:\n{}",
    frame_req.headers
  );
}
