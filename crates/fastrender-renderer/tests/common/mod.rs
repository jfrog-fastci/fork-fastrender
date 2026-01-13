#![allow(dead_code)]

use bincode::Options;
use fastrender_ipc::{
  BrowserToRenderer, CursorKind, DocumentOrigin, FrameBuffer, FrameId, RendererToBrowser, SiteKey,
  SubframeInfo, MAX_IPC_MESSAGE_BYTES,
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

pub fn net_test_lock() -> MutexGuard<'static, ()> {
  match NET_TEST_LOCK.get_or_init(|| Mutex::new(())).lock() {
    Ok(guard) => guard,
    Err(poisoned) => poisoned.into_inner(),
  }
}

#[track_caller]
pub fn try_bind_localhost(context: &str) -> Option<TcpListener> {
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
pub struct CapturedRequest {
  pub path: String,
  pub referer: Option<String>,
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

pub struct TestServer {
  addr: SocketAddr,
  captured: Arc<Mutex<Vec<CapturedRequest>>>,
  shutdown: Arc<AtomicBool>,
  handle: Option<thread::JoinHandle<()>>,
}

#[derive(Debug, Clone)]
pub struct TestResponse {
  pub status: u16,
  pub headers: Vec<(String, String)>,
  pub body: Vec<u8>,
  pub content_type: &'static str,
}

impl TestServer {
  const MAX_SERVER_LIFETIME: Duration = Duration::from_secs(15);

  pub fn start(
    context: &str,
    handler: impl Fn(&str) -> Option<(Vec<u8>, &'static str)> + Send + Sync + 'static,
  ) -> Option<Self> {
    Self::start_with(context, move |path| {
      handler(path).map(|(body, content_type)| TestResponse {
        status: 200,
        headers: Vec::new(),
        body,
        content_type,
      })
    })
  }

  pub fn start_with(
    context: &str,
    handler: impl Fn(&str) -> Option<TestResponse> + Send + Sync + 'static,
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

            let resp = match handler(captured_request.path.as_str()) {
              Some(resp) => resp,
              None => TestResponse {
                status: 404,
                headers: Vec::new(),
                body: Vec::new(),
                content_type: "text/plain",
              },
            };

            let mut header_block = format!(
              "HTTP/1.1 {} OK\r\nContent-Type: {}\r\nContent-Length: {}\r\n",
              resp.status,
              resp.content_type,
              resp.body.len()
            );
            for (name, value) in &resp.headers {
              header_block.push_str(&format!("{name}: {value}\r\n"));
            }
            header_block.push_str("Connection: close\r\n\r\n");

            let _ = stream.write_all(header_block.as_bytes());
            let _ = stream.write_all(&resp.body);
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

  pub fn url(&self, path: &str) -> String {
    format!("http://{}/{}", self.addr, path.trim_start_matches('/'))
  }

  pub fn captured(&self) -> Vec<CapturedRequest> {
    self
      .captured
      .lock()
      .unwrap_or_else(|poisoned| poisoned.into_inner())
      .clone()
  }

  pub fn wait_for_request(&self, predicate: impl Fn(&CapturedRequest) -> bool, context: &str) {
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

  pub fn shutdown_and_join(mut self) -> Vec<CapturedRequest> {
    self.shutdown.store(true, Ordering::SeqCst);
    if let Some(handle) = self.handle.take() {
      let _ = handle.join();
    }
    self.captured()
  }
}

pub fn tiny_png() -> Vec<u8> {
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

fn write_ipc_message<W: Write>(writer: &mut W, msg: &BrowserToRenderer) {
  let opts = bincode::DefaultOptions::new();
  let len = opts.serialized_size(msg).expect("size message");
  assert!(len > 0);
  assert!(len <= (u32::MAX as u64));
  assert!(len <= (MAX_IPC_MESSAGE_BYTES as u64));

  writer
    .write_all(&(len as u32).to_le_bytes())
    .expect("write length prefix");
  opts
    .serialize_into(&mut *writer, msg)
    .expect("write message payload");
  writer.flush().expect("flush");
}

fn read_ipc_message<R: Read>(reader: &mut R) -> Option<RendererToBrowser> {
  let mut len_prefix = [0u8; 4];
  if reader.read_exact(&mut len_prefix).is_err() {
    return None;
  }
  let len = u32::from_le_bytes(len_prefix) as usize;
  if len == 0 || len > MAX_IPC_MESSAGE_BYTES {
    return None;
  }
  let mut limited = reader.take(len as u64);
  let msg = bincode::DefaultOptions::new()
    .with_limit(len as u64)
    .deserialize_from::<_, RendererToBrowser>(&mut limited)
    .ok()?;
  // Consume any trailing bytes so the stream stays aligned even if decode was lenient.
  let _ = std::io::copy(&mut limited, &mut std::io::sink());
  Some(msg)
}

#[derive(Debug, Clone)]
pub struct FrameReadyWithMeta {
  pub frame_id: FrameId,
  pub buffer: FrameBuffer,
  pub subframes: Vec<SubframeInfo>,
  pub last_committed: Option<CommittedNavigation>,
  pub last_error: Option<String>,
}

#[derive(Debug, Clone)]
pub struct CommittedNavigation {
  pub url: String,
  pub base_url: Option<String>,
  pub csp: Vec<String>,
}

pub struct RendererProc {
  child: ChildKillGuard,
  stdin: ChildStdin,
  stderr: ChildStderr,
  rx: mpsc::Receiver<RendererToBrowser>,
  reader: thread::JoinHandle<()>,
}

impl RendererProc {
  pub fn spawn() -> Self {
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
      while let Some(msg) = read_ipc_message(&mut stdout) {
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

  pub fn send(&mut self, msg: &BrowserToRenderer) {
    write_ipc_message(&mut self.stdin, msg);
  }

  pub fn recv_frame_ready(&self, timeout: Duration) -> FrameReadyWithMeta {
    let deadline = Instant::now() + timeout;
    let mut last_error: Option<String> = None;
    let mut last_committed: Option<CommittedNavigation> = None;

    while Instant::now() < deadline {
      let msg = match self.rx.recv_timeout(Duration::from_millis(50)) {
        Ok(msg) => msg,
        Err(mpsc::RecvTimeoutError::Timeout) => continue,
        Err(mpsc::RecvTimeoutError::Disconnected) => break,
      };
      match msg {
        RendererToBrowser::NavigationCommitted {
          url,
          base_url,
          csp,
          ..
        } => {
          last_committed = Some(CommittedNavigation { url, base_url, csp });
        }
        RendererToBrowser::FrameReady {
          frame_id,
          buffer,
          subframes,
        } => {
          return FrameReadyWithMeta {
            frame_id,
            buffer,
            subframes,
            last_committed,
            last_error,
          };
        }
        RendererToBrowser::FramePaintPlan(plan) => {
          let frame_id = plan.frame_id;
          let subframes = plan.slots.clone();
          let buffer = match fastrender_ipc::composite_paint_plan(
            plan,
            std::iter::empty::<(&SubframeInfo, &FrameBuffer)>(),
          ) {
            Ok(buffer) => buffer,
            Err(err) => {
              last_error = Some(format!("composite_paint_plan failed: {err:?}"));
              FrameBuffer {
                width: 0,
                height: 0,
                rgba8: Vec::new(),
              }
            }
          };
          return FrameReadyWithMeta {
            frame_id,
            buffer,
            subframes,
            last_committed,
            last_error,
          };
        }
        RendererToBrowser::Error { message, .. } => last_error = Some(message),
        RendererToBrowser::SubframesDiscovered { .. }
        | RendererToBrowser::NavigationFailed { .. }
        | RendererToBrowser::HoverChanged { .. }
        | RendererToBrowser::InputAck { .. } => {}
      }
    }

    FrameReadyWithMeta {
      frame_id: FrameId(0),
      buffer: FrameBuffer {
        width: 0,
        height: 0,
        rgba8: Vec::new(),
      },
      subframes: Vec::new(),
      last_committed,
      last_error: Some(last_error.unwrap_or_else(|| "timed out waiting for FrameReady".to_string())),
    }
  }

  pub fn recv_navigation_failed(&self, timeout: Duration) -> Option<(FrameId, String, String)> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
      let msg = match self.rx.recv_timeout(Duration::from_millis(50)) {
        Ok(msg) => msg,
        Err(mpsc::RecvTimeoutError::Timeout) => continue,
        Err(mpsc::RecvTimeoutError::Disconnected) => break,
      };
      if let RendererToBrowser::NavigationFailed {
        frame_id,
        url,
        error,
      } = msg
      {
        return Some((frame_id, url, error));
      }
    }
    None
  }

  pub fn recv_hover_changed(
    &self,
    timeout: Duration,
  ) -> Option<(FrameId, u64, Option<String>, CursorKind)> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
      let msg = match self.rx.recv_timeout(Duration::from_millis(50)) {
        Ok(msg) => msg,
        Err(mpsc::RecvTimeoutError::Timeout) => continue,
        Err(mpsc::RecvTimeoutError::Disconnected) => break,
      };
      if let RendererToBrowser::HoverChanged {
        frame_id,
        seq,
        hovered_url,
        cursor,
      } = msg
      {
        return Some((frame_id, seq, hovered_url, cursor));
      }
    }
    None
  }

  pub fn shutdown(mut self) {
    write_ipc_message(&mut self.stdin, &BrowserToRenderer::Shutdown);
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

pub fn site_key_for_url(url: &str) -> SiteKey {
  let parsed = Url::parse(url).expect("parse url");
  SiteKey::Origin(DocumentOrigin::from_url(&parsed))
}
