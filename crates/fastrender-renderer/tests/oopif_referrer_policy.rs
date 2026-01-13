use bincode::Options;
use fastrender_ipc::{
  BrowserToRenderer, DocumentOrigin, FrameId, NavigationContext, ReferrerPolicy, RendererToBrowser,
  SiteKey, SubframeInfo, MAX_IPC_MESSAGE_BYTES,
};
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::process::{Child, ChildStderr, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex, MutexGuard, OnceLock};
use std::thread;
use std::time::{Duration, Instant};
use url::Url;

// Many tests spin up local TCP servers and run HTTP clients in parallel. When the test runner uses
// a very high thread count, localhost networking can get flaky (spurious connection failures).
// Serialize network-heavy tests behind a single global lock to keep CI deterministic.
static NET_TEST_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

fn net_test_lock() -> MutexGuard<'static, ()> {
  match NET_TEST_LOCK.get_or_init(|| Mutex::new(())).lock() {
    Ok(guard) => guard,
    Err(poisoned) => poisoned.into_inner(),
  }
}

#[track_caller]
fn try_bind_localhost(context: &str) -> Option<TcpListener> {
  match TcpListener::bind("127.0.0.1:0") {
    Ok(listener) => Some(listener),
    Err(err)
      if matches!(
        err.kind(),
        std::io::ErrorKind::PermissionDenied | std::io::ErrorKind::AddrNotAvailable
      ) =>
    {
      let loc = std::panic::Location::caller();
      eprintln!(
        "skipping {context} ({}:{}): cannot bind localhost in this environment: {err}",
        loc.file(),
        loc.line()
      );
      None
    }
    Err(err) => {
      let loc = std::panic::Location::caller();
      panic!("bind {context} ({}:{}): {err}", loc.file(), loc.line());
    }
  }
}

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

  let path = match Url::parse(raw_target).ok() {
    Some(url) => url.path().to_string(),
    None => raw_target
      .split_once('?')
      .map(|(before, _)| before)
      .unwrap_or(raw_target)
      .to_string(),
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
  const MAX_SERVER_LIFETIME: Duration = Duration::from_secs(15);

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
      while !shutdown_for_thread.load(Ordering::SeqCst) && start.elapsed() < Self::MAX_SERVER_LIFETIME {
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

  fn wait_for_request(&self, predicate: impl Fn(&CapturedRequest) -> bool, context: &str) {
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline {
      if self
        .captured
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
      .captured
      .lock()
      .map(|guard| guard.iter().filter(|req| predicate(req)).count())
      .unwrap_or(0);
    assert!(
      matches > 0,
      "{context}\n\nCaptured requests:\n{:#?}",
      self.captured()
    );
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
    0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44, 0x52,
    0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x02, 0x00, 0x00, 0x00, 0x90, 0x77, 0x53,
    0xde, 0x00, 0x00, 0x00, 0x0c, 0x49, 0x44, 0x41, 0x54, 0x08, 0xd7, 0x63, 0xf8, 0xff, 0xff, 0x3f,
    0x00, 0x05, 0xfe, 0x02, 0xfe, 0xdc, 0xcc, 0x59, 0xe7, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4e,
    0x44, 0xae, 0x42, 0x60, 0x82,
  ]
}

struct ChildKillGuard(Option<Child>);

impl ChildKillGuard {
  fn new(child: Child) -> Self {
    Self(Some(child))
  }

  fn take(&mut self) -> Child {
    self.0.take().expect("child already taken")
  }
}

impl Drop for ChildKillGuard {
  fn drop(&mut self) {
    if let Some(mut child) = self.0.take() {
      let _ = child.kill();
      let _ = child.wait();
    }
  }
}

struct RendererProc {
  child: ChildKillGuard,
  stdin: ChildStdin,
  stderr: ChildStderr,
  rx: mpsc::Receiver<RendererToBrowser>,
  reader: thread::JoinHandle<()>,
}

impl RendererProc {
  fn spawn() -> Self {
    let exe = env!("CARGO_BIN_EXE_fastrender-renderer");
    let child = Command::new(exe)
      .stdin(Stdio::piped())
      .stdout(Stdio::piped())
      .stderr(Stdio::piped())
      .spawn()
      .expect("spawn renderer binary");
    let mut child = ChildKillGuard::new(child);

    let stdin = child.0.as_mut().unwrap().stdin.take().expect("child stdin");
    let stdout = child.0.as_mut().unwrap().stdout.take().expect("child stdout");
    let stderr = child.0.as_mut().unwrap().stderr.take().expect("child stderr");

    let (msg_tx, msg_rx) = mpsc::channel::<RendererToBrowser>();
    let reader = thread::spawn(move || {
      let mut stdout = stdout;
      loop {
        let mut len_prefix = [0u8; 4];
        match stdout.read_exact(&mut len_prefix) {
          Ok(()) => {}
          Err(err) => {
            if err.kind() == std::io::ErrorKind::UnexpectedEof {
              break;
            }
            break;
          }
        }

        let len = u32::from_le_bytes(len_prefix) as usize;
        if len == 0 || len > MAX_IPC_MESSAGE_BYTES {
          break;
        }

        let mut limited = stdout.by_ref().take(len as u64);
        let msg = match bincode::DefaultOptions::new()
          .with_limit(len as u64)
          .deserialize_from::<_, RendererToBrowser>(&mut limited)
        {
          Ok(msg) => msg,
          Err(_) => break,
        };
        let _ = std::io::copy(&mut limited, &mut std::io::sink());
        if msg_tx.send(msg).is_err() {
          break;
        }
      }
    });

    Self {
      child,
      stdin,
      stderr,
      rx: msg_rx,
      reader,
    }
  }

  fn send(&mut self, msg: &BrowserToRenderer) {
    let opts = bincode::DefaultOptions::new();
    let len = opts.serialized_size(msg).expect("size message");
    assert!(len > 0);
    assert!(len <= (u32::MAX as u64));
    assert!(len <= (MAX_IPC_MESSAGE_BYTES as u64));
    self
      .stdin
      .write_all(&(len as u32).to_le_bytes())
      .expect("write length prefix");
    opts
      .serialize_into(&mut self.stdin, msg)
      .expect("write message payload");
    self.stdin.flush().expect("flush stdin");
  }

  fn recv_frame_ready(
    &self,
    timeout: Duration,
  ) -> (FrameId, Vec<SubframeInfo>, Option<String>) {
    let deadline = Instant::now() + timeout;
    let mut last_error: Option<String> = None;
    while Instant::now() < deadline {
      let msg = match self.rx.recv_timeout(Duration::from_millis(50)) {
        Ok(msg) => msg,
        Err(mpsc::RecvTimeoutError::Timeout) => continue,
        Err(mpsc::RecvTimeoutError::Disconnected) => break,
      };
      match msg {
        RendererToBrowser::FrameReady {
          frame_id,
          buffer: _,
          subframes,
        } => return (frame_id, subframes, last_error),
        RendererToBrowser::Error { frame_id: _, message } => last_error = Some(message),
      }
    }
    (
      FrameId(0),
      Vec::new(),
      Some(last_error.unwrap_or_else(|| "timed out waiting for FrameReady".to_string())),
    )
  }

  fn shutdown(mut self) {
    let msg = BrowserToRenderer::Shutdown;
    let opts = bincode::DefaultOptions::new();
    let len = opts.serialized_size(&msg).expect("size shutdown");
    self
      .stdin
      .write_all(&(len as u32).to_le_bytes())
      .expect("write shutdown length");
    opts
      .serialize_into(&mut self.stdin, &msg)
      .expect("write shutdown payload");
    self.stdin.flush().expect("flush shutdown");
    drop(self.stdin);

    let mut child_inner = self.child.take();
    let status = child_inner.wait().expect("wait for child exit");
    assert!(
      status.success(),
      "renderer exited with {status:?} (stderr={})",
      {
        let mut buf = String::new();
        let _ = std::io::Read::read_to_string(&mut self.stderr, &mut buf);
        buf
      }
    );
    self.reader.join().expect("join stdout reader");
  }
}

fn site_key_for_url(url: &str) -> SiteKey {
  let parsed = Url::parse(url).expect("parse url");
  SiteKey::Origin(DocumentOrigin {
    scheme: parsed.scheme().to_ascii_lowercase(),
    host: parsed.host_str().map(|h| h.to_ascii_lowercase()),
    port: parsed.port_or_known_default(),
  })
}

#[test]
fn oopif_iframe_referrerpolicy_no_referrer_omits_referer_on_child_subresource_fetches() {
  let _net_guard = net_test_lock();

  let Some(child_server) = TestServer::start(
    "oopif_iframe_referrerpolicy_no_referrer_child",
    |path| match path {
      "/frame.html" => Some((
        b"<!doctype html><html><body><img src=\"/img.png\"></body></html>".to_vec(),
        "text/html",
      )),
      "/img.png" => Some((tiny_png(), "image/png")),
      _ => None,
    },
  ) else {
    return;
  };
  let child_url = child_server.url("frame.html");
  let child_url_for_parent = child_url.clone();

  let Some(parent_server) = TestServer::start(
    "oopif_iframe_referrerpolicy_no_referrer_parent",
    move |path| match path {
      "/index.html" => Some((
        format!(
          "<!doctype html><html><body><iframe src=\"{child_url_for_parent}\" referrerpolicy=\"no-referrer\"></iframe></body></html>"
        )
        .into_bytes(),
        "text/html",
      )),
      _ => None,
    },
  ) else {
    return;
  };
  let parent_url = parent_server.url("index.html");

  let mut parent_renderer = RendererProc::spawn();
  let parent_frame = FrameId(1);
  parent_renderer.send(&BrowserToRenderer::CreateFrame {
    frame_id: parent_frame,
  });
  parent_renderer.send(&BrowserToRenderer::Navigate {
    frame_id: parent_frame,
    url: parent_url.clone(),
    context: NavigationContext {
      referrer_url: None,
      referrer_policy: ReferrerPolicy::default(),
      site_key: site_key_for_url(&parent_url),
    },
  });
  parent_renderer.send(&BrowserToRenderer::RequestRepaint {
    frame_id: parent_frame,
  });

  let (got_frame, subframes, err) = parent_renderer.recv_frame_ready(Duration::from_secs(2));
  assert_eq!(
    got_frame, parent_frame,
    "expected FrameReady for parent frame (err={err:?})"
  );
  assert!(
    !subframes.is_empty(),
    "expected parent renderer to report at least one subframe (err={err:?})"
  );
  let iframe = &subframes[0];
  assert_eq!(iframe.referrer_policy, Some(ReferrerPolicy::NoReferrer));

  let mut child_renderer = RendererProc::spawn();
  let child_frame = iframe.child;
  child_renderer.send(&BrowserToRenderer::CreateFrame {
    frame_id: child_frame,
  });
  child_renderer.send(&BrowserToRenderer::Navigate {
    frame_id: child_frame,
    url: child_url.clone(),
    context: NavigationContext::for_subframe_navigation(
      parent_url.clone(),
      ReferrerPolicy::default(),
      iframe.referrer_policy,
      site_key_for_url(&child_url),
    ),
  });
  child_renderer.send(&BrowserToRenderer::RequestRepaint {
    frame_id: child_frame,
  });

  // The child renderer should fetch /img.png without sending a Referer header due to the iframe's
  // `referrerpolicy=no-referrer` attribute.
  child_server.wait_for_request(
    |req| req.path == "/img.png",
    "expected child process to fetch /img.png",
  );
  let captured = child_server.shutdown_and_join();
  let img_requests: Vec<_> = captured.iter().filter(|r| r.path == "/img.png").collect();
  assert!(
    !img_requests.is_empty(),
    "expected at least one request for /img.png, got: {captured:?}"
  );
  for req in img_requests {
    assert_eq!(req.referer, None, "unexpected Referer header: {req:?}");
  }

  // Stop renderers and parent server.
  parent_renderer.shutdown();
  child_renderer.shutdown();
  let _ = parent_server.shutdown_and_join();
}
