use crate::test_support::net::{net_test_lock, try_bind_localhost};

use fastrender::api::{BrowserDocumentDom2, BrowserTab, BrowserTabHost, BrowserTabJsExecutor, RenderOptions};
use fastrender::debug::runtime::{with_thread_runtime_toggles, RuntimeToggles};
use fastrender::dom2::NodeId;
use fastrender::error::Result;
use fastrender::js::{EventLoop, RunLimits, ScriptElementSpec};

use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use base64::Engine;
use sha2::{Digest, Sha256};

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::{Arc, Mutex};

#[derive(Clone, Default)]
struct LogExecutor {
  log: Arc<Mutex<Vec<String>>>,
}

impl LogExecutor {
  fn take_log(&self) -> Vec<String> {
    std::mem::take(
      &mut *self
        .log
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()),
    )
  }
}

impl BrowserTabJsExecutor for LogExecutor {
  fn execute_classic_script(
    &mut self,
    script_text: &str,
    _spec: &ScriptElementSpec,
    _current_script: Option<NodeId>,
    _document: &mut BrowserDocumentDom2,
    _event_loop: &mut EventLoop<BrowserTabHost>,
  ) -> Result<()> {
    self
      .log
      .lock()
      .unwrap_or_else(|poisoned| poisoned.into_inner())
      .push(script_text.to_string());
    Ok(())
  }
}

fn read_http_request(stream: &mut TcpStream) -> (String, HashMap<String, String>) {
  let mut buf: Vec<u8> = Vec::new();
  let mut tmp = [0u8; 1024];
  loop {
    let n = stream.read(&mut tmp).expect("read request");
    if n == 0 {
      break;
    }
    buf.extend_from_slice(&tmp[..n]);
    if buf.windows(4).any(|w| w == b"\r\n\r\n") {
      break;
    }
    // Hard cap to avoid hanging on malformed clients.
    if buf.len() > 64 * 1024 {
      break;
    }
  }

  let header_end = buf
    .windows(4)
    .position(|w| w == b"\r\n\r\n")
    .unwrap_or(buf.len());
  let head = String::from_utf8_lossy(&buf[..header_end]);
  let mut lines = head.split("\r\n");
  let request_line = lines.next().unwrap_or_default();
  let path = request_line
    .split_whitespace()
    .nth(1)
    .unwrap_or_default()
    .to_string();

  let mut headers: HashMap<String, String> = HashMap::new();
  for line in lines {
    if line.is_empty() {
      break;
    }
    let Some((name, value)) = line.split_once(':') else {
      continue;
    };
    headers.insert(
      name.trim().to_ascii_lowercase(),
      value.trim().to_string(),
    );
  }

  (path, headers)
}

fn write_http_response(
  mut stream: TcpStream,
  status: &str,
  content_type: &str,
  body: &str,
  extra_headers: &[(&str, &str)],
) {
  let mut head = format!(
    "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n",
    body.as_bytes().len()
  );
  for (name, value) in extra_headers {
    head.push_str(name);
    head.push_str(": ");
    head.push_str(value);
    head.push_str("\r\n");
  }
  head.push_str("\r\n");

  stream
    .write_all(head.as_bytes())
    .expect("write response headers");
  stream
    .write_all(body.as_bytes())
    .expect("write response body");
}

#[test]
fn sri_sha256_allows_matching_digest() -> Result<()> {
  let script_url = "https://example.com/a.js";
  let script_body = "console.log('ok');";
  let digest = Sha256::digest(script_body.as_bytes());
  let b64 = BASE64_STANDARD.encode(digest);

  let html = format!(
    r#"<!doctype html><script async src="{script_url}" integrity="sha256-{b64}"></script>"#
  );

  let executor = LogExecutor::default();
  let mut tab = BrowserTab::from_html(&html, RenderOptions::default(), executor.clone())?;
  tab.register_script_source(script_url, script_body);
  tab.run_event_loop_until_idle(RunLimits::unbounded())?;

  assert_eq!(executor.take_log(), vec![script_body.to_string()]);
  Ok(())
}

#[test]
fn sri_sha256_mismatch_blocks_script_execution_without_aborting() -> Result<()> {
  let script_url = "https://example.com/a.js";
  let script_body = "console.log('ok');";
  let digest = Sha256::digest(b"wrong");
  let b64 = BASE64_STANDARD.encode(digest);

  let html = format!(
    r#"<!doctype html><script async src="{script_url}" integrity="sha256-{b64}"></script>"#
  );

  let executor = LogExecutor::default();
  let mut tab = BrowserTab::from_html(&html, RenderOptions::default(), executor.clone())?;
  tab.register_script_source(script_url, script_body);

  // SRI mismatches should behave like script load failures: no execution, but the run completes.
  tab.run_event_loop_until_idle(RunLimits::unbounded())?;

  assert_eq!(executor.take_log(), Vec::<String>::new());
  Ok(())
}

#[test]
fn crossorigin_anonymous_enforces_cors_and_blocks_on_missing_acao() -> Result<()> {
  let _net_lock = net_test_lock();
  let Some(doc_listener) = try_bind_localhost("cors script document server") else {
    return Ok(());
  };
  let Some(script_listener) = try_bind_localhost("cors script asset server") else {
    return Ok(());
  };

  let doc_addr = doc_listener.local_addr().expect("doc addr");
  let script_addr = script_listener.local_addr().expect("script addr");
  let doc_url = format!("http://{}/page.html", doc_addr);
  let script_url = format!("http://{}/script.js", script_addr);

  let captured_script_headers: Arc<Mutex<Option<HashMap<String, String>>>> = Arc::new(Mutex::new(None));
  let captured_script_headers_for_thread = Arc::clone(&captured_script_headers);

  let doc_thread = std::thread::spawn(move || {
    let (mut stream, _) = doc_listener.accept().expect("accept doc");
    let (_path, _headers) = read_http_request(&mut stream);
    let body = format!(
      r#"<!doctype html><html><head>
        <script src="{script_url}" crossorigin="anonymous"></script>
        <script>INLINE</script>
      </head><body></body></html>"#
    );
    write_http_response(stream, "200 OK", "text/html", &body, &[]);
  });

  let script_thread = std::thread::spawn(move || {
    let (mut stream, _) = script_listener.accept().expect("accept script");
    let (_path, headers) = read_http_request(&mut stream);
    *captured_script_headers_for_thread
      .lock()
      .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(headers);
    // Intentionally omit `Access-Control-Allow-Origin` so CORS enforcement blocks the script.
    write_http_response(stream, "200 OK", "application/javascript", "EXTERNAL", &[]);
  });

  let executor = LogExecutor::default();
  let toggles = Arc::new(RuntimeToggles::from_map(HashMap::from([(
    "FASTR_FETCH_ENFORCE_CORS".to_string(),
    "1".to_string(),
  )])));
  with_thread_runtime_toggles(toggles, || -> Result<()> {
    let mut tab = BrowserTab::from_html("", RenderOptions::default(), executor.clone())?;
    tab.navigate_to_url(&doc_url, RenderOptions::default())?;
    // No async/defer scripts; everything should have been handled during navigation.
    tab.run_event_loop_until_idle(RunLimits::unbounded())?;
    Ok(())
  })?;

  doc_thread.join().expect("join doc thread");
  script_thread.join().expect("join script thread");

  assert_eq!(executor.take_log(), vec!["INLINE".to_string()]);

  let headers = captured_script_headers
    .lock()
    .unwrap_or_else(|poisoned| poisoned.into_inner())
    .clone()
    .unwrap_or_default();

  assert_eq!(
    headers.get("sec-fetch-mode").map(String::as_str),
    Some("cors"),
    "expected crossorigin scripts to use CORS mode; headers={headers:?}"
  );
  assert_eq!(
    headers.get("sec-fetch-dest").map(String::as_str),
    Some("script"),
    "expected script fetch destination; headers={headers:?}"
  );
  let expected_origin = format!("http://{}", doc_addr);
  assert_eq!(
    headers.get("origin").map(String::as_str),
    Some(expected_origin.as_str()),
    "expected Origin to match document origin; headers={headers:?}"
  );

  Ok(())
}

#[test]
fn referrerpolicy_no_referrer_suppresses_referer_header_for_scripts() -> Result<()> {
  let _net_lock = net_test_lock();
  let Some(listener) = try_bind_localhost("referrerpolicy script server") else {
    return Ok(());
  };

  let addr = listener.local_addr().expect("server addr");
  let doc_url = format!("http://{}/page.html", addr);

  let captured_script_headers: Arc<Mutex<Option<HashMap<String, String>>>> = Arc::new(Mutex::new(None));
  let captured_script_headers_for_thread = Arc::clone(&captured_script_headers);

  let server_thread = std::thread::spawn(move || {
    // Handle the document request + the script request (two separate connections).
    for _ in 0..2 {
      let (mut stream, _) = listener.accept().expect("accept");
      let (path, headers) = read_http_request(&mut stream);
      if path == "/page.html" {
        let body = r#"<!doctype html><html><head>
          <script src="/script.js" referrerpolicy="no-referrer"></script>
        </head><body></body></html>"#;
        write_http_response(stream, "200 OK", "text/html", body, &[]);
      } else if path == "/script.js" {
        *captured_script_headers_for_thread
          .lock()
          .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(headers);
        write_http_response(stream, "200 OK", "application/javascript", "EXTERNAL", &[]);
      } else {
        write_http_response(stream, "404 Not Found", "text/plain", "not found", &[]);
      }
    }
  });

  let executor = LogExecutor::default();
  let mut tab = BrowserTab::from_html("", RenderOptions::default(), executor.clone())?;
  tab.navigate_to_url(&doc_url, RenderOptions::default())?;
  tab.run_event_loop_until_idle(RunLimits::unbounded())?;

  server_thread.join().expect("join server thread");

  assert_eq!(executor.take_log(), vec!["EXTERNAL".to_string()]);

  let headers = captured_script_headers
    .lock()
    .unwrap_or_else(|poisoned| poisoned.into_inner())
    .clone()
    .unwrap_or_default();

  assert!(
    !headers.contains_key("referer"),
    "expected referrerpolicy=no-referrer to suppress Referer header; got headers={headers:?}"
  );
  Ok(())
}

#[test]
fn document_referrer_policy_header_applies_to_script_requests() -> Result<()> {
  let _net_lock = net_test_lock();
  let Some(listener) = try_bind_localhost("referrer-policy header script server") else {
    return Ok(());
  };

  let addr = listener.local_addr().expect("server addr");
  let doc_url = format!("http://{}/page.html", addr);

  let captured_script_headers: Arc<Mutex<Option<HashMap<String, String>>>> = Arc::new(Mutex::new(None));
  let captured_script_headers_for_thread = Arc::clone(&captured_script_headers);

  let server_thread = std::thread::spawn(move || {
    // Handle the document request + the script request (two separate connections).
    for _ in 0..2 {
      let (mut stream, _) = listener.accept().expect("accept");
      let (path, headers) = read_http_request(&mut stream);
      if path == "/page.html" {
        let body = r#"<!doctype html><html><head>
          <script src="/script.js"></script>
        </head><body></body></html>"#;
        write_http_response(
          stream,
          "200 OK",
          "text/html",
          body,
          &[("Referrer-Policy", "no-referrer")],
        );
      } else if path == "/script.js" {
        *captured_script_headers_for_thread
          .lock()
          .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(headers);
        write_http_response(stream, "200 OK", "application/javascript", "EXTERNAL", &[]);
      } else {
        write_http_response(stream, "404 Not Found", "text/plain", "not found", &[]);
      }
    }
  });

  let executor = LogExecutor::default();
  let mut tab = BrowserTab::from_html("", RenderOptions::default(), executor.clone())?;
  tab.navigate_to_url(&doc_url, RenderOptions::default())?;
  tab.run_event_loop_until_idle(RunLimits::unbounded())?;

  server_thread.join().expect("join server thread");

  assert_eq!(executor.take_log(), vec!["EXTERNAL".to_string()]);

  let headers = captured_script_headers
    .lock()
    .unwrap_or_else(|poisoned| poisoned.into_inner())
    .clone()
    .unwrap_or_default();

  assert!(
    !headers.contains_key("referer"),
    "expected Referrer-Policy response header to suppress Referer header; got headers={headers:?}"
  );
  Ok(())
}

#[test]
fn crossorigin_use_credentials_includes_cookies_on_cross_origin_script_requests() -> Result<()> {
  let _net_lock = net_test_lock();
  let Some(doc_listener) = try_bind_localhost("cors credentials script document server") else {
    return Ok(());
  };
  let Some(script_listener) = try_bind_localhost("cors credentials script asset server") else {
    return Ok(());
  };

  let doc_addr = doc_listener.local_addr().expect("doc addr");
  let script_addr = script_listener.local_addr().expect("script addr");
  let doc_url = format!("http://{}/page.html", doc_addr);
  let anon_url = format!("http://{}/anon.js", script_addr);
  let cred_url = format!("http://{}/cred.js", script_addr);
  let expected_origin = format!("http://{}", doc_addr);

  let captured_script_headers: Arc<Mutex<HashMap<String, HashMap<String, String>>>> =
    Arc::new(Mutex::new(HashMap::new()));
  let captured_script_headers_for_thread = Arc::clone(&captured_script_headers);

  let doc_thread = std::thread::spawn(move || {
    let (mut stream, _) = doc_listener.accept().expect("accept doc");
    let (_path, _headers) = read_http_request(&mut stream);
    let body = format!(
      r#"<!doctype html><html><head>
        <script src="{anon_url}" crossorigin="anonymous"></script>
        <script src="{cred_url}" crossorigin="use-credentials"></script>
      </head><body></body></html>"#
    );
    // Host-only cookies ignore port, so a cookie set by `doc_url` can still be attached to the
    // cross-origin (different port) script request when the credentials mode is `include`.
    write_http_response(
      stream,
      "200 OK",
      "text/html",
      &body,
      &[("Set-Cookie", "session=abc; Path=/")],
    );
  });

  let script_thread = std::thread::spawn(move || {
    for _ in 0..2 {
      let (mut stream, _) = script_listener.accept().expect("accept script");
      let (path, headers) = read_http_request(&mut stream);
      captured_script_headers_for_thread
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .insert(path.clone(), headers);

      match path.as_str() {
        "/anon.js" => {
          write_http_response(
            stream,
            "200 OK",
            "application/javascript",
            "ANON",
            &[("Access-Control-Allow-Origin", expected_origin.as_str())],
          );
        }
        "/cred.js" => {
          write_http_response(
            stream,
            "200 OK",
            "application/javascript",
            "CRED",
            &[
              ("Access-Control-Allow-Origin", expected_origin.as_str()),
              ("Access-Control-Allow-Credentials", "true"),
            ],
          );
        }
        _ => {
          write_http_response(stream, "404 Not Found", "text/plain", "not found", &[]);
        }
      }
    }
  });

  let executor = LogExecutor::default();
  let toggles = Arc::new(RuntimeToggles::from_map(HashMap::from([(
    "FASTR_FETCH_ENFORCE_CORS".to_string(),
    "1".to_string(),
  )])));
  with_thread_runtime_toggles(toggles, || -> Result<()> {
    let mut tab = BrowserTab::from_html("", RenderOptions::default(), executor.clone())?;
    tab.navigate_to_url(&doc_url, RenderOptions::default())?;
    tab.run_event_loop_until_idle(RunLimits::unbounded())?;
    Ok(())
  })?;

  doc_thread.join().expect("join doc thread");
  script_thread.join().expect("join script thread");

  assert_eq!(
    executor.take_log(),
    vec!["ANON".to_string(), "CRED".to_string()]
  );

  let headers_by_path = captured_script_headers
    .lock()
    .unwrap_or_else(|poisoned| poisoned.into_inner())
    .clone();

  let anon_headers = headers_by_path
    .get("/anon.js")
    .cloned()
    .unwrap_or_default();
  let cred_headers = headers_by_path
    .get("/cred.js")
    .cloned()
    .unwrap_or_default();

  assert!(
    !anon_headers.contains_key("cookie"),
    "expected crossorigin=anonymous scripts to omit Cookie on cross-origin requests; headers={anon_headers:?}"
  );
  let cookie = cred_headers.get("cookie").cloned().unwrap_or_default();
  assert!(
    cookie.contains("session=abc"),
    "expected crossorigin=use-credentials scripts to include Cookie on cross-origin requests; headers={cred_headers:?}"
  );
  Ok(())
}

#[test]
fn crossorigin_use_credentials_blocks_without_allow_credentials_or_with_wildcard_acao() -> Result<()> {
  let _net_lock = net_test_lock();
  let Some(doc_listener) = try_bind_localhost("cors credentials failures script document server") else {
    return Ok(());
  };
  let Some(script_listener) = try_bind_localhost("cors credentials failures script asset server") else {
    return Ok(());
  };

  let doc_addr = doc_listener.local_addr().expect("doc addr");
  let script_addr = script_listener.local_addr().expect("script addr");
  let doc_url = format!("http://{}/page.html", doc_addr);
  let missing_cred_url = format!("http://{}/missing.js", script_addr);
  let wildcard_url = format!("http://{}/wildcard.js", script_addr);
  let expected_origin = format!("http://{}", doc_addr);
  let expected_origin_for_thread = expected_origin.clone();

  let captured_script_headers: Arc<Mutex<HashMap<String, HashMap<String, String>>>> =
    Arc::new(Mutex::new(HashMap::new()));
  let captured_script_headers_for_thread = Arc::clone(&captured_script_headers);

  let doc_thread = std::thread::spawn(move || {
    let (mut stream, _) = doc_listener.accept().expect("accept doc");
    let (_path, _headers) = read_http_request(&mut stream);
    let body = format!(
      r#"<!doctype html><html><head>
        <script src="{missing_cred_url}" crossorigin="use-credentials"></script>
        <script>INLINE1</script>
        <script src="{wildcard_url}" crossorigin="use-credentials"></script>
        <script>INLINE2</script>
      </head><body></body></html>"#
    );
    write_http_response(
      stream,
      "200 OK",
      "text/html",
      &body,
      &[("Set-Cookie", "session=abc; Path=/")],
    );
  });

  let script_thread = std::thread::spawn(move || {
    let expected_origin = expected_origin_for_thread;
    for _ in 0..2 {
      let (mut stream, _) = script_listener.accept().expect("accept script");
      let (path, headers) = read_http_request(&mut stream);
      captured_script_headers_for_thread
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .insert(path.clone(), headers);

      match path.as_str() {
        "/missing.js" => {
          // Missing `Access-Control-Allow-Credentials: true` must block credentialed CORS requests.
          write_http_response(
            stream,
            "200 OK",
            "application/javascript",
            "MISSING",
            &[("Access-Control-Allow-Origin", expected_origin.as_str())],
          );
        }
        "/wildcard.js" => {
          // Wildcard ACAO must be rejected for credentialed CORS requests even if ACAC is present.
          write_http_response(
            stream,
            "200 OK",
            "application/javascript",
            "WILDCARD",
            &[
              ("Access-Control-Allow-Origin", "*"),
              ("Access-Control-Allow-Credentials", "true"),
            ],
          );
        }
        _ => {
          write_http_response(stream, "404 Not Found", "text/plain", "not found", &[]);
        }
      }
    }
  });

  let executor = LogExecutor::default();
  let toggles = Arc::new(RuntimeToggles::from_map(HashMap::from([(
    "FASTR_FETCH_ENFORCE_CORS".to_string(),
    "1".to_string(),
  )])));
  with_thread_runtime_toggles(toggles, || -> Result<()> {
    let mut tab = BrowserTab::from_html("", RenderOptions::default(), executor.clone())?;
    tab.navigate_to_url(&doc_url, RenderOptions::default())?;
    tab.run_event_loop_until_idle(RunLimits::unbounded())?;
    Ok(())
  })?;

  doc_thread.join().expect("join doc thread");
  script_thread.join().expect("join script thread");

  assert_eq!(
    executor.take_log(),
    vec!["INLINE1".to_string(), "INLINE2".to_string()],
    "expected credentialed CORS failures to block external scripts without aborting parsing"
  );

  let headers_by_path = captured_script_headers
    .lock()
    .unwrap_or_else(|poisoned| poisoned.into_inner())
    .clone();

  for path in ["/missing.js", "/wildcard.js"] {
    let headers = headers_by_path.get(path).cloned().unwrap_or_default();
    assert_eq!(
      headers.get("sec-fetch-mode").map(String::as_str),
      Some("cors"),
      "expected {path} to use CORS mode; headers={headers:?}"
    );
    assert_eq!(
      headers.get("origin").map(String::as_str),
      Some(expected_origin.as_str()),
      "expected {path} Origin header to match document origin; headers={headers:?}"
    );
    let cookie = headers.get("cookie").cloned().unwrap_or_default();
    assert!(
      cookie.contains("session=abc"),
      "expected credentialed script requests to include Cookie; {path} headers={headers:?}"
    );
  }

  Ok(())
}
