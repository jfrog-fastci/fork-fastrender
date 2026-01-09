use fastrender::debug::runtime::{with_thread_runtime_toggles, RuntimeToggles};
#[cfg(feature = "disk_cache")]
use fastrender::resource::DiskCachingFetcher;
use fastrender::resource::{
  origin_from_url, CachingFetcher, FetchDestination, FetchRequest, HttpFetcher, ResourceFetcher,
};
use crate::test_support;
use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};
use test_support::net::net_test_lock;
use url::Url;

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

fn origin_from_request(headers: &str) -> Option<String> {
  let mut origin: Option<String> = None;
  let mut referer: Option<String> = None;

  for line in headers.lines() {
    let Some((name, value)) = line.split_once(':') else {
      continue;
    };
    let name = name.trim();
    let value = value.trim();
    if name.eq_ignore_ascii_case("origin") {
      origin = Some(value.to_string());
    } else if name.eq_ignore_ascii_case("referer") {
      referer = Some(value.to_string());
    }
  }

  origin.or_else(|| {
    let referer = referer?;
    let parsed = Url::parse(&referer).ok()?;
    let host = parsed.host_str()?;
    let mut out = format!("{}://{}", parsed.scheme(), host);
    if let Some(port) = parsed.port() {
      out.push_str(&format!(":{port}"));
    }
    Some(out)
  })
}

struct OriginEchoServer {
  url: String,
  hits: Arc<AtomicUsize>,
  shutdown: Arc<AtomicBool>,
  join: Option<thread::JoinHandle<()>>,
}

impl OriginEchoServer {
  fn start(context: &str) -> Option<Self> {
    let listener = try_bind_localhost(context)?;
    let addr = listener.local_addr().ok()?;
    let url = format!("http://{}/font.woff2", addr);
    let hits = Arc::new(AtomicUsize::new(0));
    let shutdown = Arc::new(AtomicBool::new(false));

    let thread_hits = Arc::clone(&hits);
    let thread_shutdown = Arc::clone(&shutdown);
    let join = thread::spawn(move || {
      let _ = listener.set_nonblocking(true);
      while !thread_shutdown.load(Ordering::SeqCst) {
        match listener.accept() {
          Ok((mut stream, _addr)) => {
            let req = read_http_headers(&mut stream).unwrap_or_default();
            let origin = origin_from_request(&req);
            let body = b"ok";
            let allow_origin = origin.as_deref().unwrap_or("*");
            let response = format!(
              "HTTP/1.1 200 OK\r\nContent-Type: font/woff2\r\nAccess-Control-Allow-Origin: {allow_origin}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
              body.len()
            );
            let _ = stream.write_all(response.as_bytes());
            let _ = stream.write_all(body);
            thread_hits.fetch_add(1, Ordering::SeqCst);
          }
          Err(err) if err.kind() == io::ErrorKind::WouldBlock => {
            thread::sleep(Duration::from_millis(5));
          }
          Err(_) => break,
        }
      }
    });

    Some(Self {
      url,
      hits,
      shutdown,
      join: Some(join),
    })
  }
}

impl Drop for OriginEchoServer {
  fn drop(&mut self) {
    self.shutdown.store(true, Ordering::SeqCst);
    if let Some(join) = self.join.take() {
      let _ = join.join();
    }
  }
}

fn wait_for_hits(server: &OriginEchoServer, expected: usize, context: &str) {
  let deadline = Instant::now() + Duration::from_secs(2);
  while server.hits.load(Ordering::SeqCst) < expected && Instant::now() < deadline {
    thread::sleep(Duration::from_millis(5));
  }
  // Give the accept loop a brief chance to observe any additional requests so we don't race
  // against the non-blocking server thread.
  thread::sleep(Duration::from_millis(10));
  assert_eq!(server.hits.load(Ordering::SeqCst), expected, "{context}");
}

#[test]
fn in_memory_cache_partitions_cors_mode_by_request_origin_when_enforced() {
  let _net_guard = net_test_lock();
  let Some(server) =
    OriginEchoServer::start("in_memory_cache_partitions_cors_mode_by_request_origin_when_enforced")
  else {
    return;
  };

  let mut raw = HashMap::new();
  raw.insert("FASTR_FETCH_ENFORCE_CORS".to_string(), "1".to_string());
  let toggles = Arc::new(RuntimeToggles::from_map(raw));

  let url = server.url.clone();
  with_thread_runtime_toggles(toggles, || {
    let fetcher = CachingFetcher::new(HttpFetcher::new());

    for destination in [FetchDestination::Font, FetchDestination::ImageCors] {
      let req_a = FetchRequest::new(&url, destination).with_referrer_url("http://a.test/");
      let req_b = FetchRequest::new(&url, destination).with_referrer_url("http://b.test/");

      let a = fetcher.fetch_with_request(req_a).expect("origin A fetch");
      let b = fetcher.fetch_with_request(req_b).expect("origin B fetch");
      assert_eq!(a.bytes, b"ok");
      assert_eq!(b.bytes, b"ok");
      assert_eq!(
        a.access_control_allow_origin.as_deref(),
        Some("http://a.test")
      );
      assert_eq!(
        b.access_control_allow_origin.as_deref(),
        Some("http://b.test")
      );
    }
  });

  wait_for_hits(
    &server,
    4,
    "cache should be partitioned by origin for CORS-mode resources under FASTR_FETCH_ENFORCE_CORS",
  );
}

#[test]
fn in_memory_cache_partitions_cors_mode_by_request_origin_when_not_enforced() {
  let _net_guard = net_test_lock();
  let Some(server) = OriginEchoServer::start(
    "in_memory_cache_partitions_cors_mode_by_request_origin_when_not_enforced",
  ) else {
    return;
  };

  let mut raw = HashMap::new();
  raw.insert("FASTR_FETCH_ENFORCE_CORS".to_string(), "0".to_string());
  let toggles = Arc::new(RuntimeToggles::from_map(raw));

  let url = server.url.clone();
  with_thread_runtime_toggles(toggles, || {
    let fetcher = CachingFetcher::new(HttpFetcher::new());

    for destination in [FetchDestination::Font, FetchDestination::ImageCors] {
      let req_a = FetchRequest::new(&url, destination).with_referrer_url("http://a.test/");
      let req_b = FetchRequest::new(&url, destination).with_referrer_url("http://b.test/");

      let a = fetcher.fetch_with_request(req_a).expect("origin A fetch");
      let b = fetcher.fetch_with_request(req_b).expect("origin B fetch");
      assert_eq!(a.bytes, b"ok");
      assert_eq!(b.bytes, b"ok");
      assert_eq!(
        a.access_control_allow_origin.as_deref(),
        Some("http://a.test")
      );
      assert_eq!(
        b.access_control_allow_origin.as_deref(),
        Some("http://b.test")
      );
    }
  });

  wait_for_hits(
    &server,
    4,
    "cache should be partitioned by origin for CORS-mode resources when FASTR_FETCH_ENFORCE_CORS is disabled",
  );
}

#[test]
fn in_memory_cache_partitions_cors_mode_by_client_origin_even_when_referrer_differs() {
  let _net_guard = net_test_lock();
  let Some(server) = OriginEchoServer::start(
    "in_memory_cache_partitions_cors_mode_by_client_origin_even_when_referrer_differs",
  ) else {
    return;
  };

  let mut raw = HashMap::new();
  raw.insert("FASTR_FETCH_ENFORCE_CORS".to_string(), "1".to_string());
  let toggles = Arc::new(RuntimeToggles::from_map(raw));

  let url = server.url.clone();
  with_thread_runtime_toggles(toggles, || {
    let fetcher = CachingFetcher::new(HttpFetcher::new());
    let client_origin = origin_from_url("http://client.test/").expect("client origin");

    for destination in [FetchDestination::Font, FetchDestination::ImageCors] {
      let req_a = FetchRequest::new(&url, destination)
        .with_client_origin(&client_origin)
        .with_referrer_url("http://a.test/style.css");
      let req_b = FetchRequest::new(&url, destination)
        .with_client_origin(&client_origin)
        .with_referrer_url("http://b.test/style.css");

      let a = fetcher.fetch_with_request(req_a).expect("origin A fetch");
      let b = fetcher.fetch_with_request(req_b).expect("origin B fetch");
      assert_eq!(a.bytes, b"ok");
      assert_eq!(b.bytes, b"ok");
      assert_eq!(
        a.access_control_allow_origin.as_deref(),
        Some("http://client.test")
      );
      assert_eq!(
        b.access_control_allow_origin.as_deref(),
        Some("http://client.test")
      );
    }
  });

  wait_for_hits(
    &server,
    2,
    "cache should be keyed by initiator origin when a client origin is supplied (referrer changes must not create new CORS cache partitions)",
  );
}

#[cfg(feature = "disk_cache")]
#[test]
fn disk_cache_partitions_cors_mode_by_request_origin_when_enforced() {
  let _net_guard = net_test_lock();
  let Some(server) =
    OriginEchoServer::start("disk_cache_partitions_cors_mode_by_request_origin_when_enforced")
  else {
    return;
  };

  let mut raw = HashMap::new();
  raw.insert("FASTR_FETCH_ENFORCE_CORS".to_string(), "1".to_string());
  let toggles = Arc::new(RuntimeToggles::from_map(raw));

  let tmp = tempfile::tempdir().expect("tempdir");
  let url = server.url.clone();
  with_thread_runtime_toggles(toggles, || {
    let disk_a = DiskCachingFetcher::new(HttpFetcher::new(), tmp.path());
    let disk_b = DiskCachingFetcher::new(HttpFetcher::new(), tmp.path());

    for destination in [FetchDestination::Font, FetchDestination::ImageCors] {
      let req_a = FetchRequest::new(&url, destination).with_referrer_url("http://a.test/");
      let first = disk_a.fetch_with_request(req_a).expect("origin A fetch");
      assert_eq!(first.bytes, b"ok");
      assert_eq!(
        first.access_control_allow_origin.as_deref(),
        Some("http://a.test")
      );

      // Second fetcher instance ensures the second request consults disk (not the in-memory
      // cache).
      let req_b = FetchRequest::new(&url, destination).with_referrer_url("http://b.test/");
      let second = disk_b.fetch_with_request(req_b).expect("origin B fetch");
      assert_eq!(second.bytes, b"ok");
      assert_eq!(
        second.access_control_allow_origin.as_deref(),
        Some("http://b.test")
      );
    }
  });

  wait_for_hits(
    &server,
    4,
    "disk cache should be partitioned by origin for CORS-mode resources under FASTR_FETCH_ENFORCE_CORS",
  );
}

#[cfg(feature = "disk_cache")]
#[test]
fn disk_cache_partitions_cors_mode_by_request_origin_when_not_enforced() {
  let _net_guard = net_test_lock();
  let Some(server) =
    OriginEchoServer::start("disk_cache_partitions_cors_mode_by_request_origin_when_not_enforced")
  else {
    return;
  };

  let mut raw = HashMap::new();
  raw.insert("FASTR_FETCH_ENFORCE_CORS".to_string(), "0".to_string());
  let toggles = Arc::new(RuntimeToggles::from_map(raw));

  let tmp = tempfile::tempdir().expect("tempdir");
  let url = server.url.clone();
  with_thread_runtime_toggles(toggles, || {
    let disk_a = DiskCachingFetcher::new(HttpFetcher::new(), tmp.path());
    let disk_b = DiskCachingFetcher::new(HttpFetcher::new(), tmp.path());

    for destination in [FetchDestination::Font, FetchDestination::ImageCors] {
      let req_a = FetchRequest::new(&url, destination).with_referrer_url("http://a.test/");
      let first = disk_a.fetch_with_request(req_a).expect("origin A fetch");
      assert_eq!(first.bytes, b"ok");
      assert_eq!(
        first.access_control_allow_origin.as_deref(),
        Some("http://a.test")
      );

      let req_b = FetchRequest::new(&url, destination).with_referrer_url("http://b.test/");
      let second = disk_b.fetch_with_request(req_b).expect("origin B fetch");
      assert_eq!(second.bytes, b"ok");
      assert_eq!(
        second.access_control_allow_origin.as_deref(),
        Some("http://b.test")
      );
    }
  });

  wait_for_hits(
    &server,
    4,
    "disk cache should be partitioned by origin for CORS-mode resources when FASTR_FETCH_ENFORCE_CORS is disabled",
  );
}
