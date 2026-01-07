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
  Some(parts.next()?.to_string())
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

            let (status, content_type, mut body) = match path.as_str() {
              "/img.png" => ("200 OK", "image/png", minimal_png().to_vec()),
              "/frame.html" => (
                "200 OK",
                "text/html; charset=utf-8",
                b"<!doctype html><body>frame</body>".to_vec(),
              ),
              "/style.css" => ("200 OK", "text/css; charset=utf-8", b"body { }".to_vec()),
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
