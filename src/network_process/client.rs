use crate::error::{Error, Result};
#[cfg(feature = "direct_websocket")]
use crate::ipc::websocket as websocket_ipc;
use crate::resource::{FetchedResource, ResourceFetcher};
use getrandom::getrandom;
use std::io::BufRead;
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
#[cfg(feature = "direct_websocket")]
use tungstenite::client::IntoClientRequest;
#[cfg(feature = "direct_websocket")]
use tungstenite::protocol::{CloseFrame, Message as TungsteniteMessage};
#[cfg(feature = "direct_websocket")]
use tungstenite::stream::MaybeTlsStream;
#[cfg(feature = "direct_websocket")]
use tungstenite::Error as TungsteniteError;

use super::ipc;

const ENV_NETWORK_AUTH_TOKEN: &str = "FASTR_NETWORK_AUTH_TOKEN";
const ENV_NETWORK_AUTH_TOKEN_DEBUG: &str = "FASTR_NETWORK_AUTH_TOKEN_DEBUG";

fn env_flag_truthy(key: &str) -> bool {
  let Ok(raw) = std::env::var(key) else {
    return false;
  };
  let raw = raw.trim();
  if raw.is_empty() {
    return false;
  }
  !matches!(
    raw.to_ascii_lowercase().as_str(),
    "0" | "false" | "no" | "off"
  )
}

#[derive(Clone, PartialEq, Eq)]
struct NetworkAuthToken(Arc<str>);

impl NetworkAuthToken {
  fn generate() -> Result<Self> {
    let mut bytes = [0u8; AUTH_TOKEN_BYTES];
    getrandom(&mut bytes)
      .map_err(|err| Error::Other(format!("failed to generate auth token: {err}")))?;
    Ok(Self(Arc::from(hex_encode(&bytes))))
  }

  fn as_str(&self) -> &str {
    self.0.as_ref()
  }
}

impl std::fmt::Debug for NetworkAuthToken {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    if env_flag_truthy(ENV_NETWORK_AUTH_TOKEN_DEBUG) {
      f.write_str(self.as_str())
    } else {
      f.write_str("<redacted>")
    }
  }
}

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

/// Spawn the `network` subprocess and return a handle to manage it.
///
/// This is a convenience wrapper that panics on failure (suitable for tests).
/// Call [`try_spawn_network_process`] to handle errors explicitly.
pub fn spawn_network_process(config: NetworkProcessConfig) -> NetworkProcessHandle {
  try_spawn_network_process(config).expect("spawn network process") // fastrender-allow-unwrap
}

/// Fallible variant of [`spawn_network_process`].
pub fn try_spawn_network_process(config: NetworkProcessConfig) -> Result<NetworkProcessHandle> {
  let binary_path = resolve_network_binary_path(&config)?;

  let auth_token = NetworkAuthToken::generate()?;

  let mut cmd = Command::new(binary_path);
  cmd.arg("--bind").arg("127.0.0.1:0");
  // Avoid exposing the token via `ps` argv output; the child process also accepts the env var.
  cmd.env(ENV_NETWORK_AUTH_TOKEN, auth_token.as_str());
  cmd.stdin(Stdio::null());
  cmd.stdout(Stdio::piped());
  if config.inherit_stderr {
    cmd.stderr(Stdio::inherit());
  } else {
    cmd.stderr(Stdio::null());
  }

  // Defense-in-depth: prevent leaking unrelated file descriptors into the exec'd network process.
  //
  // On macOS, avoid `CommandExt::pre_exec` because it forces a fork/exec spawn path (bypassing
  // `posix_spawn`) and is unsafe in multithreaded parents.
  #[cfg(all(unix, target_os = "linux"))]
  {
    use std::os::unix::process::CommandExt as _;
    let keep = [0, 1, 2];
    unsafe {
      cmd.pre_exec(move || {
        // Ensure the network process is killed if the parent disappears (defense-in-depth; avoids
        // leaving a privileged background process running unexpectedly).
        let _ = crate::sandbox::linux_set_parent_death_signal();

        crate::sandbox::set_cloexec_on_fds_except(&keep)
      });
    }
  }

  let mut child = cmd.spawn().map_err(Error::Io)?;
  let pid = child.id();

  let stdout = child
    .stdout
    .take()
    .ok_or_else(|| Error::Other("network subprocess stdout was not captured".to_string()))?;

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
  auth_token: NetworkAuthToken,
  child: Mutex<Option<Child>>,
}

impl NetworkProcessHandle {
  /// Create a new IPC client object configured for an untrusted renderer.
  ///
  /// This is the least-privileged role and should be preferred whenever the caller might run in (or
  /// on behalf of) an untrusted renderer process.
  pub fn connect_client(&self) -> NetworkClient {
    self.connect_client_with_role(ipc::ClientRole::Renderer)
  }

  /// Create a new IPC client object with an explicit [`ipc::ClientRole`].
  pub fn connect_client_with_role(&self, role: ipc::ClientRole) -> NetworkClient {
    NetworkClient {
      addr: self.addr,
      connect_timeout: self.connect_timeout,
      auth_token: self.auth_token.clone(),
      role,
    }
  }

  /// Create a privileged IPC client configured for the trusted browser.
  pub fn connect_browser_client(&self) -> NetworkClient {
    self.connect_client_with_role(ipc::ClientRole::Browser)
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
    self.auth_token.as_str()
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

    // Best-effort graceful shutdown first. We keep this bounded (short timeouts) because `Drop`
    // should never hang indefinitely.
    let _ = TcpStream::connect_timeout(&self.addr, self.connect_timeout).and_then(|stream| {
      let _ = stream.set_nodelay(true);
      let _ = stream.set_read_timeout(Some(Duration::from_millis(200)));
      let _ = stream.set_write_timeout(Some(Duration::from_millis(200)));
      let mut conn = ipc::NetworkClient::new(stream);

      let _ = conn.send_request(&ipc::NetworkRequest::Hello {
        token: self.auth_token.as_str().to_string(),
        role: ipc::ClientRole::Browser,
      });
      let _ = conn.recv_response::<ipc::NetworkResponse>();

      let _ = conn.send_request(&ipc::NetworkRequest::Shutdown);
      let _ = conn.recv_response::<ipc::NetworkResponse>();
      Ok(())
    });

    // Give the child a brief window to exit on its own after receiving Shutdown.
    let deadline = Instant::now() + Duration::from_millis(200);
    loop {
      match child.try_wait() {
        Ok(Some(_status)) => return,
        Ok(None) => {
          if Instant::now() >= deadline {
            break;
          }
          std::thread::sleep(Duration::from_millis(10));
        }
        Err(_) => break,
      }
    }

    // Then hard kill if still running. Use a bounded wait so dropping the handle is non-hanging.
    let _ = crate::process_supervision::RunningChild::new(child).kill_and_wait();
  }
}

/// Client factory for network-process services (resource fetcher, WebSocket backend, downloads).
#[derive(Debug, Clone)]
pub struct NetworkClient {
  addr: SocketAddr,
  connect_timeout: Duration,
  auth_token: NetworkAuthToken,
  role: ipc::ClientRole,
}

impl NetworkClient {
  /// Client role used when authenticating to the network process.
  pub fn role(&self) -> ipc::ClientRole {
    self.role
  }

  /// Return an IPC-backed [`ResourceFetcher`] that forwards requests to the network process.
  pub fn resource_fetcher(&self) -> Arc<dyn ResourceFetcher> {
    Arc::new(IpcResourceFetcher {
      addr: self.addr,
      connect_timeout: self.connect_timeout,
      auth_token: self.auth_token.clone(),
      role: self.role,
    })
  }

  /// Return a WebSocket backend.
  ///
  /// Today this is a direct (in-process) backend; it is expected to be replaced by an IPC-backed
  /// implementation as multiprocess network isolation is built out.
  pub fn websocket_backend(&self) -> Arc<dyn WebSocketBackend> {
    #[cfg(feature = "direct_websocket")]
    {
      Arc::new(DirectWebSocketBackend)
    }
    #[cfg(not(feature = "direct_websocket"))]
    {
      Arc::new(DisabledWebSocketBackend)
    }
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
  auth_token: NetworkAuthToken,
  role: ipc::ClientRole,
}

impl IpcResourceFetcher {
  fn round_trip(&self, req: ipc::NetworkRequest) -> Result<ipc::NetworkResponse> {
    let mut stream =
      TcpStream::connect_timeout(&self.addr, self.connect_timeout).map_err(Error::Io)?;
    stream.set_nodelay(true).map_err(Error::Io)?;
    let mut conn = ipc::NetworkClient::new(stream);

    conn
      .send_request(&ipc::NetworkRequest::Hello {
        token: self.auth_token.as_str().to_string(),
        role: self.role,
      })
      .map_err(Error::Io)?;
    let hello_ack: ipc::NetworkResponse = conn.recv_response().map_err(Error::Io)?;
    match hello_ack {
      ipc::NetworkResponse::HelloAck => {}
      other => {
        return Err(Error::Other(format!(
          "network process returned unexpected response to Hello: {other:?}"
        )))
      }
    }

    conn.send_request(&req).map_err(Error::Io)?;
    let res: ipc::NetworkResponse = conn.recv_response().map_err(Error::Io)?;
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
      ipc::NetworkResponse::Error { error } => Err(Error::Other(format!(
        "network process fetch failed for {url}: {error}"
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
  Close { code: u16, reason: Option<String> },
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

#[cfg(not(feature = "direct_websocket"))]
#[derive(Debug)]
struct DisabledWebSocketBackend;

#[cfg(not(feature = "direct_websocket"))]
impl WebSocketBackend for DisabledWebSocketBackend {
  fn connect(&self, _url: &str, _protocols: &[String]) -> Result<Box<dyn WebSocketStream>> {
    Err(Error::Other(
      "WebSocket backend is unavailable (direct_websocket feature disabled; rebuild with --features direct_websocket)"
        .to_string(),
    ))
  }
}

#[cfg(feature = "direct_websocket")]
struct DirectWebSocketBackend;

#[cfg(feature = "direct_websocket")]
struct DirectWebSocketStream {
  socket: tungstenite::WebSocket<tungstenite::stream::MaybeTlsStream<std::net::TcpStream>>,
  protocol: String,
}

#[cfg(feature = "direct_websocket")]
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

#[cfg(feature = "direct_websocket")]
fn map_tungstenite_err(err: TungsteniteError) -> Error {
  match err {
    TungsteniteError::Io(err) => Error::Io(err),
    other => Error::Other(other.to_string()),
  }
}

#[cfg(feature = "direct_websocket")]
fn validate_ws_subprotocol_handshake_response(
  requested_protocols: &[String],
  response_headers: &http::HeaderMap,
) -> Result<String> {
  let mut values = response_headers.get_all("Sec-WebSocket-Protocol").iter();
  let Some(value) = values.next() else {
    // No protocol selected.
    return Ok(String::new());
  };

  // Multiple protocol headers are invalid for WebSocket subprotocol negotiation.
  if values.next().is_some() {
    return Err(Error::Other("invalid websocket subprotocol".to_string()));
  }

  let value = value
    .to_str()
    .map_err(|_| Error::Other("invalid websocket subprotocol".to_string()))?;
  if value.is_empty() {
    return Err(Error::Other("invalid websocket subprotocol".to_string()));
  }

  // The server's Sec-WebSocket-Protocol must be a single token (no commas/whitespace).
  if value.bytes().any(|b| b == b',' || b.is_ascii_whitespace()) {
    return Err(Error::Other("invalid websocket subprotocol".to_string()));
  }

  if requested_protocols.is_empty() {
    // If no subprotocols were requested, the server must not select one.
    return Err(Error::Other("invalid websocket subprotocol".to_string()));
  }

  if !requested_protocols.iter().any(|p| p == value) {
    return Err(Error::Other("invalid websocket subprotocol".to_string()));
  }

  Ok(value.to_string())
}

#[cfg(feature = "direct_websocket")]
impl WebSocketBackend for DirectWebSocketBackend {
  fn connect(&self, url: &str, protocols: &[String]) -> Result<Box<dyn WebSocketStream>> {
    let parsed = websocket_ipc::validate_and_normalize_url(url)
      .map_err(|err| Error::Other(err.to_string()))?;
    let mut req = parsed
      .clone()
      .into_client_request()
      .map_err(|err| Error::Other(err.to_string()))?;
    if !protocols.is_empty() {
      let joined = protocols.join(", ");
      req.headers_mut().insert(
        "Sec-WebSocket-Protocol",
        http::HeaderValue::from_str(&joined).map_err(|err| Error::Other(err.to_string()))?,
      );
    }

    let connect_timeout = if cfg!(test) {
      Duration::from_secs(1)
    } else {
      Duration::from_secs(5)
    };
    let (socket, response) =
      crate::resource::websocket::connect_with_timeout(&parsed, req, connect_timeout)
        .map_err(map_tungstenite_err)?;

    // The network process backend is intended to provide a blocking WebSocket stream abstraction.
    // Clear the handshake/connection timeouts so normal reads/writes do not spuriously fail once the
    // connection is established.
    match socket.get_ref() {
      MaybeTlsStream::Plain(stream) => {
        let _ = stream.set_read_timeout(None);
        let _ = stream.set_write_timeout(None);
      }
      MaybeTlsStream::Rustls(stream) => {
        let _ = stream.get_ref().set_read_timeout(None);
        let _ = stream.get_ref().set_write_timeout(None);
      }
      #[allow(unreachable_patterns)]
      _ => {}
    }

    let protocol = validate_ws_subprotocol_handshake_response(protocols, response.headers())?;

    Ok(Box::new(DirectWebSocketStream { socket, protocol }))
  }
}

#[cfg(feature = "direct_websocket")]
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

    self
      .socket
      .write_message(msg)
      .map_err(map_tungstenite_err)?;
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

#[cfg(all(test, feature = "direct_websocket"))]
mod tests {
  use super::*;
  use crate::testing::{net_test_lock, try_bind_localhost};
  use std::time::Instant;

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

  #[test]
  fn websocket_rejects_unrequested_protocol_selected_by_server() -> Result<()> {
    let _lock = net_test_lock();
    let Some(listener) = try_bind_localhost(
      "network_process_websocket_rejects_unrequested_protocol_selected_by_server",
    ) else {
      return Ok(());
    };
    listener.set_nonblocking(true).expect("set_nonblocking");
    let addr = listener.local_addr().expect("local_addr");

    let server = std::thread::spawn(move || {
      let deadline = Instant::now() + Duration::from_secs(5);
      loop {
        match listener.accept() {
          Ok((stream, _)) => {
            let mut stream = stream;
            let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
            let _ = stream.set_write_timeout(Some(Duration::from_secs(5)));

            // Deliberately violate RFC6455 by returning a comma-separated protocol list even though
            // the client requested a single protocol.
            let _ws = tungstenite::accept_hdr(stream, |_req, mut resp| {
              resp
                .headers_mut()
                .insert("Sec-WebSocket-Protocol", "chat, superchat".parse().unwrap());
              Ok(resp)
            })
            .expect("accept websocket");
            break;
          }
          Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
            if Instant::now() >= deadline {
              panic!("accept timed out");
            }
            std::thread::sleep(Duration::from_millis(10));
          }
          Err(e) => panic!("accept failed: {e}"),
        }
      }
    });

    let backend = DirectWebSocketBackend;
    let url = format!("ws://{addr}/");
    let res = backend.connect(&url, &[String::from("chat")]);
    assert!(
      res.is_err(),
      "expected invalid subprotocol negotiation to fail"
    );

    server.join().expect("server thread panicked");
    Ok(())
  }

  #[test]
  fn websocket_protocol_is_set_from_server_handshake_response() -> Result<()> {
    let _lock = net_test_lock();
    let Some(listener) = try_bind_localhost(
      "network_process_websocket_protocol_is_set_from_server_handshake_response",
    ) else {
      return Ok(());
    };
    listener.set_nonblocking(true).expect("set_nonblocking");
    let addr = listener.local_addr().expect("local_addr");

    let server = std::thread::spawn(move || {
      let deadline = Instant::now() + Duration::from_secs(5);
      loop {
        match listener.accept() {
          Ok((stream, _)) => {
            let mut stream = stream;
            let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
            let _ = stream.set_write_timeout(Some(Duration::from_secs(5)));

            let mut ws = tungstenite::accept_hdr(stream, |_req, mut resp| {
              resp
                .headers_mut()
                .insert("Sec-WebSocket-Protocol", "superchat".parse().unwrap());
              Ok(resp)
            })
            .expect("accept websocket");
            let _ = ws.close(None);
            break;
          }
          Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
            if Instant::now() >= deadline {
              panic!("accept timed out");
            }
            std::thread::sleep(Duration::from_millis(10));
          }
          Err(e) => panic!("accept failed: {e}"),
        }
      }
    });

    let backend = DirectWebSocketBackend;
    let url = format!("ws://{addr}/");
    let mut ws = backend.connect(&url, &[String::from("chat"), String::from("superchat")])?;
    assert_eq!(ws.protocol(), "superchat");
    let _ = ws.close(Some(1000), None);

    server.join().expect("server thread panicked");
    Ok(())
  }

  #[test]
  fn websocket_rejects_protocol_when_none_were_requested() -> Result<()> {
    let _lock = net_test_lock();
    let Some(listener) =
      try_bind_localhost("network_process_websocket_rejects_protocol_when_none_were_requested")
    else {
      return Ok(());
    };
    listener.set_nonblocking(true).expect("set_nonblocking");
    let addr = listener.local_addr().expect("local_addr");

    let server = std::thread::spawn(move || {
      let deadline = Instant::now() + Duration::from_secs(5);
      loop {
        match listener.accept() {
          Ok((stream, _)) => {
            let mut stream = stream;
            let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
            let _ = stream.set_write_timeout(Some(Duration::from_secs(5)));

            let _ws = tungstenite::accept_hdr(stream, |_req, mut resp| {
              resp
                .headers_mut()
                .insert("Sec-WebSocket-Protocol", "chat".parse().unwrap());
              Ok(resp)
            })
            .expect("accept websocket");
            break;
          }
          Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
            if Instant::now() >= deadline {
              panic!("accept timed out");
            }
            std::thread::sleep(Duration::from_millis(10));
          }
          Err(e) => panic!("accept failed: {e}"),
        }
      }
    });

    let backend = DirectWebSocketBackend;
    let url = format!("ws://{addr}/");
    let res = backend.connect(&url, &[]);
    assert!(
      res.is_err(),
      "expected unrequested protocol selection to fail"
    );

    server.join().expect("server thread panicked");
    Ok(())
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
