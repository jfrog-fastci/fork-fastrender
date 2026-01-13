use crate::error::{Error, Result};
use crate::ipc::websocket as websocket_ipc;
use crate::resource::{FetchedResource, ResourceFetcher};
use getrandom::getrandom;
use std::io::BufRead;
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tungstenite::client::IntoClientRequest;
use tungstenite::protocol::{CloseFrame, Message as TungsteniteMessage};
use tungstenite::Error as TungsteniteError;

use super::ipc;

/// Configuration for spawning the `network` subprocess.
#[derive(Debug, Clone)]
pub struct NetworkProcessConfig {
  /// Explicit path to the `network` binary.
  ///
  /// When unset, `spawn_network_process` tries (in order):
  /// - `CARGO_BIN_EXE_network` (set by Cargo for integration tests)
  /// - `../network` relative to the current executable (common in `target/debug/deps/*` layouts)
  pub binary_path: Option<PathBuf>,
  /// Maximum time to wait for the network process to report its listening address.
  pub startup_timeout: Duration,
  /// How long to wait when establishing a new IPC connection to the network process.
  pub connect_timeout: Duration,
  /// Whether to inherit the child's stderr instead of discarding it.
  pub inherit_stderr: bool,
}

impl Default for NetworkProcessConfig {
  fn default() -> Self {
    Self {
      binary_path: None,
      startup_timeout: Duration::from_secs(5),
      connect_timeout: Duration::from_secs(5),
      inherit_stderr: true,
    }
  }
}

fn exe_with_platform_suffix(stem: &str) -> String {
  if cfg!(windows) {
    format!("{stem}.exe")
  } else {
    stem.to_string()
  }
}

fn resolve_network_binary_path(config: &NetworkProcessConfig) -> Result<PathBuf> {
  if let Some(path) = &config.binary_path {
    return Ok(path.clone());
  }

  if let Some(path) = std::env::var_os("CARGO_BIN_EXE_network") {
    return Ok(PathBuf::from(path));
  }

  // Best-effort fallback for non-test callers: derive `target/<profile>/network` from the current
  // executable path (which is often `target/<profile>/deps/<test-binary>` when running tests).
  let exe = std::env::current_exe().map_err(Error::Io)?;
  let exe_dir = exe.parent().ok_or_else(|| {
    Error::Other("failed to resolve network binary: current_exe has no parent dir".to_string())
  })?;
  let profile_dir = if exe_dir
    .file_name()
    .is_some_and(|name| name == std::ffi::OsStr::new("deps"))
  {
    exe_dir.parent().ok_or_else(|| {
      Error::Other("failed to resolve network binary: deps dir has no parent".to_string())
    })?
  } else {
    exe_dir
  };
  let candidate = profile_dir.join(exe_with_platform_suffix("network"));
  if candidate.exists() {
    return Ok(candidate);
  }

  Err(Error::Other(
    "failed to resolve network binary path; set NetworkProcessConfig::binary_path or CARGO_BIN_EXE_network"
      .to_string(),
  ))
}

const AUTH_TOKEN_BYTES: usize = 32;

fn hex_encode(bytes: &[u8]) -> String {
  const HEX: &[u8; 16] = b"0123456789abcdef";
  let mut out = String::with_capacity(bytes.len() * 2);
  for &b in bytes {
    out.push(HEX[(b >> 4) as usize] as char);
    out.push(HEX[(b & 0x0F) as usize] as char);
  }
  out
}

fn generate_auth_token() -> Result<Arc<str>> {
  let mut bytes = [0u8; AUTH_TOKEN_BYTES];
  getrandom(&mut bytes).map_err(|err| Error::Other(format!("failed to generate auth token: {err}")))?;
  Ok(Arc::from(hex_encode(&bytes)))
}

/// Spawn the `network` subprocess and return a handle to manage it.
///
/// This is a convenience wrapper that panics on failure (suitable for tests).
/// Call [`try_spawn_network_process`] to handle errors explicitly.
pub fn spawn_network_process(config: NetworkProcessConfig) -> NetworkProcessHandle {
  try_spawn_network_process(config).expect("spawn network process")
}

/// Fallible variant of [`spawn_network_process`].
pub fn try_spawn_network_process(config: NetworkProcessConfig) -> Result<NetworkProcessHandle> {
  let binary_path = resolve_network_binary_path(&config)?;

  let auth_token = generate_auth_token()?;

  let mut cmd = Command::new(binary_path);
  cmd.arg("--bind").arg("127.0.0.1:0");
  cmd.arg("--auth-token").arg(auth_token.as_ref());
  cmd.stdin(Stdio::null());
  cmd.stdout(Stdio::piped());
  if config.inherit_stderr {
    cmd.stderr(Stdio::inherit());
  } else {
    cmd.stderr(Stdio::null());
  }

  let mut child = cmd.spawn().map_err(Error::Io)?;
  let pid = child.id();

  let stdout = child.stdout.take().ok_or_else(|| {
    Error::Other("network subprocess stdout was not captured".to_string())
  })?;

  let (tx, rx) = std::sync::mpsc::channel::<std::io::Result<String>>();
  std::thread::spawn(move || {
    let mut reader = std::io::BufReader::new(stdout);
    let mut line = String::new();
    let res = reader.read_line(&mut line).map(|_| line);
    let _ = tx.send(res);
  });

  let line = rx
    .recv_timeout(config.startup_timeout)
    .map_err(|_| Error::Other("timed out waiting for network subprocess handshake".to_string()))?
    .map_err(Error::Io)?;

  let addr: SocketAddr = line.trim().parse().map_err(|err| {
    Error::Other(format!(
      "network subprocess reported invalid socket address {line:?}: {err}"
    ))
  })?;

  Ok(NetworkProcessHandle {
    addr,
    connect_timeout: config.connect_timeout,
    pid,
    auth_token,
    child: Mutex::new(Some(child)),
  })
}

/// A handle that owns the spawned network process.
///
/// Dropping the handle attempts to terminate the subprocess (best-effort).
pub struct NetworkProcessHandle {
  addr: SocketAddr,
  connect_timeout: Duration,
  pid: u32,
  auth_token: Arc<str>,
  child: Mutex<Option<Child>>,
}

impl NetworkProcessHandle {
  /// Create a new IPC client object.
  pub fn connect_client(&self) -> NetworkClient {
    NetworkClient {
      addr: self.addr,
      connect_timeout: self.connect_timeout,
      auth_token: self.auth_token.clone(),
    }
  }

  /// Address the network process is listening on.
  pub fn addr(&self) -> SocketAddr {
    self.addr
  }

  /// PID of the spawned network process.
  pub fn pid(&self) -> u32 {
    self.pid
  }

  /// Authentication token required to connect to this network process instance.
  pub fn auth_token(&self) -> &str {
    self.auth_token.as_ref()
  }
}

impl Drop for NetworkProcessHandle {
  fn drop(&mut self) {
    let child = self
      .child
      .lock()
      .unwrap_or_else(|poisoned| poisoned.into_inner())
      .take();
    let Some(mut child) = child else {
      return;
    };

    // Best-effort graceful shutdown first.
    let _ = TcpStream::connect_timeout(&self.addr, self.connect_timeout).and_then(|mut stream| {
      stream
        .set_nodelay(true)
        .unwrap_or_else(|_| ()); // ignore
      // Authenticate before issuing the shutdown command.
      let _ = ipc::write_request_frame(
        &mut stream,
        &ipc::NetworkRequest::Hello {
          token: self.auth_token.to_string(),
        },
      );
      let _ = ipc::write_request_frame(&mut stream, &ipc::NetworkRequest::Shutdown);
      Ok(())
    });

    // Then hard kill if still running.
    let _ = child.kill();
    let _ = child.wait();
  }
}

/// Client factory for network-process services (resource fetcher, WebSocket backend, downloads).
#[derive(Debug, Clone)]
pub struct NetworkClient {
  addr: SocketAddr,
  connect_timeout: Duration,
  auth_token: Arc<str>,
}

impl NetworkClient {
  /// Return an IPC-backed [`ResourceFetcher`] that forwards requests to the network process.
  pub fn resource_fetcher(&self) -> Arc<dyn ResourceFetcher> {
    Arc::new(IpcResourceFetcher {
      addr: self.addr,
      connect_timeout: self.connect_timeout,
      auth_token: self.auth_token.clone(),
    })
  }

  /// Return a WebSocket backend.
  ///
  /// Today this is a direct (in-process) backend; it is expected to be replaced by an IPC-backed
  /// implementation as multiprocess network isolation is built out.
  pub fn websocket_backend(&self) -> Arc<dyn WebSocketBackend> {
    Arc::new(DirectWebSocketBackend)
  }

  /// Create a simple download client backed by this client's [`ResourceFetcher`].
  pub fn download_client(&self) -> DownloadClient {
    DownloadClient {
      fetcher: self.resource_fetcher(),
    }
  }
}

/// IPC-backed resource fetcher used by [`NetworkClient`].
#[derive(Debug, Clone)]
pub struct IpcResourceFetcher {
  addr: SocketAddr,
  connect_timeout: Duration,
  auth_token: Arc<str>,
}

impl IpcResourceFetcher {
  fn round_trip(&self, req: ipc::NetworkRequest) -> Result<ipc::NetworkResponse> {
    let mut stream =
      TcpStream::connect_timeout(&self.addr, self.connect_timeout).map_err(Error::Io)?;
    stream.set_nodelay(true).map_err(Error::Io)?;

    ipc::write_request_frame(
      &mut stream,
      &ipc::NetworkRequest::Hello {
        token: self.auth_token.to_string(),
      },
    )
    .map_err(Error::Io)?;
    let hello_ack: ipc::NetworkResponse = ipc::read_response_frame(&mut stream).map_err(Error::Io)?;
    match hello_ack {
      ipc::NetworkResponse::HelloAck => {}
      other => {
        return Err(Error::Other(format!(
          "network process returned unexpected response to Hello: {other:?}"
        )))
      }
    }

    ipc::write_request_frame(&mut stream, &req).map_err(Error::Io)?;
    let res: ipc::NetworkResponse = ipc::read_response_frame(&mut stream).map_err(Error::Io)?;
    Ok(res)
  }
}

impl ResourceFetcher for IpcResourceFetcher {
  fn fetch(&self, url: &str) -> Result<FetchedResource> {
    let res = self.round_trip(ipc::NetworkRequest::Fetch {
      url: url.to_string(),
    })?;

    match res {
      ipc::NetworkResponse::FetchOk { resource } => resource.into_fetched(),
      ipc::NetworkResponse::Error { message } => Err(Error::Other(format!(
        "network process fetch failed for {url}: {message}"
      ))),
      other => Err(Error::Other(format!(
        "network process returned unexpected response to fetch: {other:?}"
      ))),
    }
  }
}

/// WebSocket messages exposed by [`WebSocketBackend`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WebSocketMessage {
  Text(String),
  Binary(Vec<u8>),
  Ping(Vec<u8>),
  Pong(Vec<u8>),
  Close {
    code: u16,
    reason: Option<String>,
  },
}

/// An abstract WebSocket connection.
pub trait WebSocketStream: Send {
  fn send(&mut self, message: WebSocketMessage) -> Result<()>;
  fn recv(&mut self) -> Result<WebSocketMessage>;
  fn close(&mut self, code: Option<u16>, reason: Option<String>) -> Result<()>;
  fn protocol(&self) -> &str;
}

/// WebSocket backend abstraction.
pub trait WebSocketBackend: Send + Sync {
  fn connect(&self, url: &str, protocols: &[String]) -> Result<Box<dyn WebSocketStream>>;
}

struct DirectWebSocketBackend;

struct DirectWebSocketStream {
  socket: tungstenite::WebSocket<tungstenite::stream::MaybeTlsStream<std::net::TcpStream>>,
  protocol: String,
}

fn normalize_close_code_for_frame(code: u16) -> u16 {
  // Renderer-provided close codes are untrusted. Never allow codes that RFC 6455 forbids on the wire
  // (e.g. 1005/1006/1015) or out-of-range values that tungstenite may reject.
  //
  // Invalid codes are mapped to 1000 (normal closure) so the network process can still honor the
  // close request without risking protocol errors.
  if websocket_ipc::is_valid_close_code(code) {
    code
  } else {
    1000
  }
}

fn map_tungstenite_err(err: TungsteniteError) -> Error {
  match err {
    TungsteniteError::Io(err) => Error::Io(err),
    other => Error::Other(other.to_string()),
  }
}

impl WebSocketBackend for DirectWebSocketBackend {
  fn connect(&self, url: &str, protocols: &[String]) -> Result<Box<dyn WebSocketStream>> {
    let mut req = url
      .into_client_request()
      .map_err(|err| Error::Other(err.to_string()))?;
    if !protocols.is_empty() {
      let joined = protocols.join(", ");
      req.headers_mut().insert(
        "Sec-WebSocket-Protocol",
        http::HeaderValue::from_str(&joined).map_err(|err| Error::Other(err.to_string()))?,
      );
    }

    let (socket, response) = tungstenite::connect(req).map_err(map_tungstenite_err)?;
    let protocol = response
      .headers()
      .get("sec-websocket-protocol")
      .and_then(|h| h.to_str().ok())
      .unwrap_or("")
      .to_string();

    Ok(Box::new(DirectWebSocketStream { socket, protocol }))
  }
}

impl WebSocketStream for DirectWebSocketStream {
  fn send(&mut self, message: WebSocketMessage) -> Result<()> {
    let msg = match message {
      WebSocketMessage::Text(text) => TungsteniteMessage::Text(text),
      WebSocketMessage::Binary(bytes) => TungsteniteMessage::Binary(bytes),
      WebSocketMessage::Ping(bytes) => TungsteniteMessage::Ping(bytes),
      WebSocketMessage::Pong(bytes) => TungsteniteMessage::Pong(bytes),
      WebSocketMessage::Close { code, reason } => {
        let code = normalize_close_code_for_frame(code);
        TungsteniteMessage::Close(Some(CloseFrame {
          code: tungstenite::protocol::frame::coding::CloseCode::from(code),
          reason: reason.unwrap_or_default().into(),
        }))
      }
    };

    self.socket.write_message(msg).map_err(map_tungstenite_err)?;
    Ok(())
  }

  fn recv(&mut self) -> Result<WebSocketMessage> {
    let msg = self.socket.read_message().map_err(map_tungstenite_err)?;
    Ok(match msg {
      TungsteniteMessage::Text(text) => WebSocketMessage::Text(text),
      TungsteniteMessage::Binary(bytes) => WebSocketMessage::Binary(bytes),
      TungsteniteMessage::Ping(bytes) => WebSocketMessage::Ping(bytes),
      TungsteniteMessage::Pong(bytes) => WebSocketMessage::Pong(bytes),
      TungsteniteMessage::Close(frame) => {
        let frame = frame.unwrap_or_else(|| CloseFrame {
          code: tungstenite::protocol::frame::coding::CloseCode::Normal,
          reason: "".into(),
        });
        WebSocketMessage::Close {
          code: frame.code.into(),
          reason: (!frame.reason.is_empty()).then_some(frame.reason.to_string()),
        }
      }
      TungsteniteMessage::Frame(_) => {
        return Err(Error::Other(
          "unexpected raw WebSocket frame from tungstenite".to_string(),
        ));
      }
    })
  }

  fn close(&mut self, code: Option<u16>, reason: Option<String>) -> Result<()> {
    let frame = code.map(|code| {
      let code = normalize_close_code_for_frame(code);
      CloseFrame {
        code: tungstenite::protocol::frame::coding::CloseCode::from(code),
        reason: reason.unwrap_or_default().into(),
      }
    });
    self.socket.close(frame).map_err(map_tungstenite_err)?;
    Ok(())
  }

  fn protocol(&self) -> &str {
    &self.protocol
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn normalize_close_code_rejects_reserved_codes() {
    assert_eq!(normalize_close_code_for_frame(1000), 1000);
    assert_eq!(normalize_close_code_for_frame(3000), 3000);
    assert_eq!(normalize_close_code_for_frame(4999), 4999);

    // Reserved/invalid codes must never be sent on the wire.
    assert_eq!(normalize_close_code_for_frame(1005), 1000);
    assert_eq!(normalize_close_code_for_frame(1006), 1000);
    assert_eq!(normalize_close_code_for_frame(1015), 1000);
    assert_eq!(normalize_close_code_for_frame(0), 1000);
    assert_eq!(normalize_close_code_for_frame(2000), 1000);
    assert_eq!(normalize_close_code_for_frame(5000), 1000);
  }
}

/// Simple download helper used by consumers of [`NetworkClient`].
#[derive(Clone)]
pub struct DownloadClient {
  fetcher: Arc<dyn ResourceFetcher>,
}

impl DownloadClient {
  pub fn download_to_path(&self, url: &str, path: impl AsRef<Path>) -> Result<()> {
    let res = self.fetcher.fetch(url)?;
    std::fs::write(path, &res.bytes).map_err(Error::Io)?;
    Ok(())
  }
}
