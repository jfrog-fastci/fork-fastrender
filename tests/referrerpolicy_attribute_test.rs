use fastrender::api::{FastRender, FastRenderConfig, RenderOptions};
use fastrender::resource::{HttpFetcher, ResourceFetcher};
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

mod test_support;
use test_support::net::try_bind_localhost;

#[derive(Debug, Clone)]
struct CapturedRequest {
  path: String,
  referer: Option<String>,
}

fn read_http_request(stream: &mut TcpStream) -> std::io::Result<String> {
  stream.set_read_timeout(Some(Duration::from_millis(500)))?;
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
        if data.len() > 64 * 1024 {
          break;
        }
      }
      Err(err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
      Err(err) => return Err(err),
    }
  }
  Ok(String::from_utf8_lossy(&data).into_owned())
}

fn parse_path_and_referer(request: &str) -> CapturedRequest {
  let raw_target = request
    .lines()
    .next()
    .and_then(|line| line.split_whitespace().nth(1))
    .unwrap_or("/");

  // Requests can be either origin-form (`GET /path HTTP/1.1`) or absolute-form
  // (`GET http://host/path HTTP/1.1`). Normalize both into just the path so the
  // assertions can stay stable across HTTP backends.
  let path = match url::Url::parse(raw_target).ok() {
    Some(url) => url.path().to_string(),
    None => raw_target.split_once('?').map(|(before, _)| before).unwrap_or(raw_target).to_string(),
  };

  let mut referer = None;
  for line in request.lines() {
    let line = line.trim_end_matches('\r');
    let Some((name, value)) = line.split_once(':') else {
      continue;
    };
    if name.trim().eq_ignore_ascii_case("referer") {
      referer = Some(value.trim().to_string());
      break;
    }
  }

  CapturedRequest { path, referer }
}

struct TestServer {
  addr: SocketAddr,
  captured: Arc<Mutex<Vec<CapturedRequest>>>,
  shutdown: Arc<AtomicBool>,
  handle: Option<thread::JoinHandle<()>>,
}

impl TestServer {
  fn start(
    context: &str,
    handler: impl Fn(&str) -> Option<(Vec<u8>, &'static str)> + Send + Sync + 'static,
  ) -> Option<Self> {
    let listener = try_bind_localhost(context)?;
    listener
      .set_nonblocking(true)
      .expect("set nonblocking listener");
    let addr = listener.local_addr().expect("server addr");
    let captured = Arc::new(Mutex::new(Vec::<CapturedRequest>::new()));
    let captured_for_thread = Arc::clone(&captured);
    let shutdown = Arc::new(AtomicBool::new(false));
    let shutdown_for_thread = Arc::clone(&shutdown);
    let handler = Arc::new(handler);

    let handle = thread::spawn(move || {
      let start = Instant::now();
      while !shutdown_for_thread.load(Ordering::SeqCst) && start.elapsed() < Duration::from_secs(5) {
        match listener.accept() {
          Ok((mut stream, _)) => {
            let request = read_http_request(&mut stream).unwrap_or_default();
            let captured_request = parse_path_and_referer(&request);
            captured_for_thread
              .lock()
              .unwrap_or_else(|poisoned| poisoned.into_inner())
              .push(captured_request.clone());

            let (status, body, content_type) = match handler(captured_request.path.as_str()) {
              Some((body, content_type)) => (200u16, body, content_type),
              None => (404u16, Vec::new(), "text/plain"),
            };
            let response = format!(
              "HTTP/1.1 {status} OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
              body.len()
            );
            let _ = stream.write_all(response.as_bytes());
            let _ = stream.write_all(&body);
            let _ = stream.flush();
          }
          Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
            thread::sleep(Duration::from_millis(5));
          }
          Err(err) => panic!("accept failed: {err}"),
        }
      }
    });

    Some(Self {
      addr,
      captured,
      shutdown,
      handle: Some(handle),
    })
  }

  fn origin(&self) -> String {
    format!("http://{}/", self.addr)
  }

  fn url(&self, path: &str) -> String {
    format!("http://{}/{}", self.addr, path.trim_start_matches('/'))
  }

  fn captured(&self) -> Vec<CapturedRequest> {
    self
      .captured
      .lock()
      .unwrap_or_else(|poisoned| poisoned.into_inner())
      .clone()
  }

  fn shutdown_and_join(mut self) -> Vec<CapturedRequest> {
    self.shutdown.store(true, Ordering::SeqCst);
    if let Some(handle) = self.handle.take() {
      let _ = handle.join();
    }
    self.captured()
  }
}

fn tiny_png() -> Vec<u8> {
  // 1x1 RGB PNG.
  vec![
    0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48,
    0x44, 0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x02, 0x00, 0x00,
    0x00, 0x90, 0x77, 0x53, 0xde, 0x00, 0x00, 0x00, 0x0c, 0x49, 0x44, 0x41, 0x54, 0x08,
    0xd7, 0x63, 0xf8, 0xff, 0xff, 0x3f, 0x00, 0x05, 0xfe, 0x02, 0xfe, 0xdc, 0xcc, 0x59,
    0xe7, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4e, 0x44, 0xae, 0x42, 0x60, 0x82,
  ]
}

#[test]
fn img_referrerpolicy_no_referrer_omits_referer_header() {
  let Some(server) = TestServer::start(
    "img_referrerpolicy_no_referrer_omits_referer_header",
    |path| match path {
      "/img.png" => Some((tiny_png(), "image/png")),
      _ => None,
    },
  ) else {
    return;
  };

  let html = r#"<!doctype html>
    <html><body>
      <img src="/img.png" referrerpolicy="no-referrer" width="1" height="1">
    </body></html>"#;
  let document_url = server.url("index.html");

  let fetcher: Arc<dyn ResourceFetcher> =
    Arc::new(HttpFetcher::new().with_timeout(Duration::from_secs(2)));
  let mut renderer =
    FastRender::with_config_and_fetcher(FastRenderConfig::default(), Some(fetcher)).unwrap();
  renderer
    .render_html_with_stylesheets(html, &document_url, RenderOptions::new().with_viewport(16, 16))
    .unwrap();

  let captured = server.shutdown_and_join();
  let img_requests: Vec<_> = captured.iter().filter(|r| r.path == "/img.png").collect();
  assert!(
    !img_requests.is_empty(),
    "expected at least one request for /img.png, got: {captured:?}"
  );
  for req in img_requests {
    assert_eq!(req.referer, None, "unexpected Referer header: {req:?}");
  }
}

#[test]
fn img_referrerpolicy_overrides_document_referrer_policy() {
  let Some(server) = TestServer::start(
    "img_referrerpolicy_overrides_document_referrer_policy",
    |path| match path {
      "/img.png" => Some((tiny_png(), "image/png")),
      _ => None,
    },
  ) else {
    return;
  };

  let html = r#"<!doctype html>
    <html><head>
      <meta name="referrer" content="no-referrer">
    </head><body>
      <img src="/img.png" referrerpolicy="origin" width="1" height="1">
    </body></html>"#;
  let document_url = server.url("index.html");

  let fetcher: Arc<dyn ResourceFetcher> =
    Arc::new(HttpFetcher::new().with_timeout(Duration::from_secs(2)));
  let mut renderer =
    FastRender::with_config_and_fetcher(FastRenderConfig::default(), Some(fetcher)).unwrap();
  renderer
    .render_html_with_stylesheets(html, &document_url, RenderOptions::new().with_viewport(16, 16))
    .unwrap();

  let expected_origin = server.origin();
  let captured = server.shutdown_and_join();
  let img_requests: Vec<_> = captured.iter().filter(|r| r.path == "/img.png").collect();
  assert!(
    !img_requests.is_empty(),
    "expected at least one request for /img.png, got: {captured:?}"
  );
  for req in img_requests {
    assert_eq!(req.referer.as_deref(), Some(expected_origin.as_str()));
  }
}

#[test]
fn iframe_referrerpolicy_overrides_document_referrer_policy() {
  let Some(server) = TestServer::start(
    "iframe_referrerpolicy_overrides_document_referrer_policy",
    |path| match path {
      "/frame.html" => Some((b"<!doctype html><html><body>frame</body></html>".to_vec(), "text/html")),
      _ => None,
    },
  ) else {
    return;
  };

  let html = r#"<!doctype html>
    <html><head>
      <meta name="referrer" content="no-referrer">
    </head><body style="margin:0">
      <iframe src="/frame.html" referrerpolicy="origin" style="width:10px; height:10px; border:0"></iframe>
    </body></html>"#;
  let document_url = server.url("index.html");

  let fetcher: Arc<dyn ResourceFetcher> =
    Arc::new(HttpFetcher::new().with_timeout(Duration::from_secs(2)));
  let mut renderer =
    FastRender::with_config_and_fetcher(FastRenderConfig::default(), Some(fetcher)).unwrap();
  renderer
    .render_html_with_stylesheets(html, &document_url, RenderOptions::new().with_viewport(32, 32))
    .unwrap();

  let expected_origin = server.origin();
  let captured = server.shutdown_and_join();
  let frame_requests: Vec<_> = captured.iter().filter(|r| r.path == "/frame.html").collect();
  assert!(
    !frame_requests.is_empty(),
    "expected iframe navigation request, got: {captured:?}"
  );
  for req in frame_requests {
    assert_eq!(req.referer.as_deref(), Some(expected_origin.as_str()));
  }
}

#[test]
fn iframe_src_referrerpolicy_applies_to_iframe_subresource_requests() {
  let Some(server) = TestServer::start(
    "iframe_src_referrerpolicy_applies_to_iframe_subresource_requests",
    |path| match path {
      "/frame.html" => Some((
        b"<!doctype html><html><body><img src='/img.png' width='1' height='1'></body></html>"
          .to_vec(),
        "text/html",
      )),
      "/img.png" => Some((tiny_png(), "image/png")),
      _ => None,
    },
  ) else {
    return;
  };

  let html = r#"<!doctype html>
    <html><head>
      <meta name="referrer" content="no-referrer">
    </head><body style="margin:0">
      <iframe src="/frame.html" referrerpolicy="origin" style="width:10px; height:10px; border:0"></iframe>
    </body></html>"#;
  let document_url = server.url("index.html");

  let fetcher: Arc<dyn ResourceFetcher> =
    Arc::new(HttpFetcher::new().with_timeout(Duration::from_secs(2)));
  let mut renderer =
    FastRender::with_config_and_fetcher(FastRenderConfig::default(), Some(fetcher)).unwrap();
  renderer
    .render_html_with_stylesheets(html, &document_url, RenderOptions::new().with_viewport(32, 32))
    .unwrap();

  let expected_origin = server.origin();
  let captured = server.shutdown_and_join();
  let img_requests: Vec<_> = captured.iter().filter(|r| r.path == "/img.png").collect();
  assert!(
    !img_requests.is_empty(),
    "expected at least one request for /img.png, got: {captured:?}"
  );
  for req in img_requests {
    assert_eq!(req.referer.as_deref(), Some(expected_origin.as_str()));
  }
}

#[test]
fn link_stylesheet_referrerpolicy_overrides_document_policy_and_applies_to_imports() {
  let Some(server) = TestServer::start(
    "link_stylesheet_referrerpolicy_overrides_document_policy_and_applies_to_imports",
    |path| match path {
      "/style.css" => Some((
        b"@import url('/import.css'); body { color: rgb(1, 2, 3); }".to_vec(),
        "text/css",
      )),
      "/import.css" => Some((b"body { background: rgb(0, 0, 0); }".to_vec(), "text/css")),
      _ => None,
    },
  ) else {
    return;
  };

  let html = r#"<!doctype html>
    <html><head>
      <meta name="referrer" content="no-referrer">
      <link rel="stylesheet" href="/style.css" referrerpolicy="origin">
    </head><body>ok</body></html>"#;
  let document_url = server.url("index.html");

  let fetcher: Arc<dyn ResourceFetcher> =
    Arc::new(HttpFetcher::new().with_timeout(Duration::from_secs(2)));
  let mut renderer =
    FastRender::with_config_and_fetcher(FastRenderConfig::default(), Some(fetcher)).unwrap();
  renderer
    .render_html_with_stylesheets(html, &document_url, RenderOptions::new().with_viewport(32, 32))
    .unwrap();

  let expected_origin = server.origin();
  let captured = server.shutdown_and_join();
  for path in ["/style.css", "/import.css"] {
    let requests: Vec<_> = captured.iter().filter(|r| r.path == path).collect();
    assert!(
      !requests.is_empty(),
      "expected at least one request for {path}, got: {captured:?}"
    );
    for req in requests {
      assert_eq!(req.referer.as_deref(), Some(expected_origin.as_str()));
    }
  }
}

#[test]
fn iframe_srcdoc_referrerpolicy_applies_to_srcdoc_subresource_requests() {
  let Some(server) = TestServer::start(
    "iframe_srcdoc_referrerpolicy_applies_to_srcdoc_subresource_requests",
    |path| match path {
      "/img.png" => Some((tiny_png(), "image/png")),
      _ => None,
    },
  ) else {
    return;
  };

  let html = r#"<!doctype html>
    <html><head>
      <meta name="referrer" content="no-referrer">
    </head><body style="margin:0">
      <iframe
        srcdoc="<img src='/img.png' width='1' height='1'>"
        referrerpolicy="origin"
        style="width:10px; height:10px; border:0"
      ></iframe>
    </body></html>"#;
  let document_url = server.url("index.html");

  let fetcher: Arc<dyn ResourceFetcher> =
    Arc::new(HttpFetcher::new().with_timeout(Duration::from_secs(2)));
  let mut renderer =
    FastRender::with_config_and_fetcher(FastRenderConfig::default(), Some(fetcher)).unwrap();
  renderer
    .render_html_with_stylesheets(html, &document_url, RenderOptions::new().with_viewport(32, 32))
    .unwrap();

  let expected_origin = server.origin();
  let captured = server.shutdown_and_join();
  let img_requests: Vec<_> = captured.iter().filter(|r| r.path == "/img.png").collect();
  assert!(
    !img_requests.is_empty(),
    "expected at least one request for /img.png, got: {captured:?}"
  );
  for req in img_requests {
    assert_eq!(req.referer.as_deref(), Some(expected_origin.as_str()));
  }
}
