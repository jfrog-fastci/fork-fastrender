//! WebSocket bindings (`WebSocket` class) for the `vm-js` Window realm.
//!
//! This is a minimal, deterministic implementation intended to unblock real-world scripts that
//! expect WebSockets to work (including `ws://` from non-secure contexts).
//!
//! Design notes:
//! - Connection I/O is performed on a dedicated thread per WebSocket.
//! - Network callbacks are delivered by queueing `EventLoop` tasks via
//!   [`crate::js::ExternalTaskQueueHandle`].
//! - The JS-visible object is a non-DOM `EventTarget` (inherits from `EventTarget.prototype`) and
//!   dispatches `open`/`message`/`error`/`close` events via `dispatchEvent`.

use crate::js::{ExternalTaskQueueHandle, TaskSource};
use crate::js::url_resolve::{resolve_url, UrlResolveError};
use crate::js::window_blob;
use crate::js::window_realm::{
  is_secure_context_for_document_url, WindowRealmHost, WindowRealmUserData, EVENT_TARGET_HOST_TAG,
};
use crate::js::window_timers::{
  event_loop_mut_from_hooks, vm_error_to_event_loop_error, VmJsEventLoopHooks,
};
use crate::ipc::websocket::{
  WebSocketCommand, WebSocketConnectParams, WebSocketEvent,
  MAX_WEBSOCKET_CLOSE_REASON_BYTES as MAX_WEBSOCKET_CLOSE_REASON_BYTES_U32,
  MAX_WEBSOCKET_MESSAGE_BYTES as MAX_WEBSOCKET_MESSAGE_BYTES_U32,
  MAX_WEBSOCKET_PROTOCOL_BYTES as MAX_WEBSOCKET_PROTOCOL_BYTES_U32,
  MAX_WEBSOCKET_PROTOCOLS as MAX_WEBSOCKET_PROTOCOLS_U32,
  MAX_WEBSOCKET_URL_BYTES as MAX_WEBSOCKET_URL_BYTES_U32,
};
use crate::resource::ResourceFetcher;
use crate::ipc::IpcError;
use std::borrow::Cow;
use std::char::decode_utf16;
use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{IpAddr, SocketAddr, ToSocketAddrs};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex, OnceLock};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use tungstenite::client::IntoClientRequest;
use tungstenite::protocol::{CloseFrame, Message};
use vm_js::iterator::{self, CloseCompletionKind};
use vm_js::{
  GcObject, GcString, Heap, HostSlots, NativeConstructId, NativeFunctionId, PropertyDescriptor,
  PropertyKey, PropertyKind, Realm, RealmId, Scope, Value, Vm, VmError, VmHost, VmHostHooks,
  WeakGcObject,
};

// IPC validation limits are defined as `u32` in `crate::ipc::websocket` because they are part of the
// multiprocess framing contract. The JS bindings and VM helpers typically operate on `usize` byte
// counts, so normalize them here to avoid pervasive casts at call sites.
const MAX_WEBSOCKET_URL_BYTES: usize = MAX_WEBSOCKET_URL_BYTES_U32 as usize;
const MAX_WEBSOCKET_PROTOCOLS: usize = MAX_WEBSOCKET_PROTOCOLS_U32 as usize;
const MAX_WEBSOCKET_PROTOCOL_BYTES: usize = MAX_WEBSOCKET_PROTOCOL_BYTES_U32 as usize;
const MAX_WEBSOCKET_MESSAGE_BYTES: usize = MAX_WEBSOCKET_MESSAGE_BYTES_U32 as usize;
const MAX_WEBSOCKET_CLOSE_REASON_BYTES: usize = MAX_WEBSOCKET_CLOSE_REASON_BYTES_U32 as usize;

const SLOT_ENV_ID: usize = 0;

const ENV_ID_KEY: &str = "__fastrender_websocket_env_id";
const WS_ID_KEY: &str = "__fastrender_websocket_id";
// Internal tombstone properties used after the backing Rust socket entry is removed.
//
// These must not collide with any spec-defined instance properties.
const WS_TOMBSTONE_URL_KEY: &str = "__fastrender_websocket_tombstone_url";
const WS_TOMBSTONE_PROTOCOL_KEY: &str = "__fastrender_websocket_tombstone_protocol";
const WS_TOMBSTONE_READY_STATE_KEY: &str = "__fastrender_websocket_tombstone_ready_state";
const WS_TOMBSTONE_BUFFERED_AMOUNT_KEY: &str = "__fastrender_websocket_tombstone_buffered_amount";

// Brand WebSocket wrappers as platform objects via HostSlots so structuredClone rejects them.
const WEBSOCKET_HOST_TAG: u64 = 0x5745_4253_4F43_4B54; // "WEBSOCKT"
pub const WS_CONNECTING: u16 = 0;
pub const WS_OPEN: u16 = 1;
pub const WS_CLOSING: u16 = 2;
pub const WS_CLOSED: u16 = 3;

/// Hard upper bound on the total number of bytes that may be queued for sending (`bufferedAmount`)
/// per WebSocket.
///
/// This prevents untrusted scripts from enqueueing multi-GiB send queues even with a per-message
/// size limit.
const MAX_WEBSOCKET_BUFFERED_AMOUNT_BYTES: usize = 16 * 1024 * 1024;
const MAX_WEBSOCKET_URL_BYTES_USIZE: usize = MAX_WEBSOCKET_URL_BYTES as usize;
const MAX_WEBSOCKET_PROTOCOLS_USIZE: usize = MAX_WEBSOCKET_PROTOCOLS as usize;
const MAX_WEBSOCKET_PROTOCOL_BYTES_USIZE: usize = MAX_WEBSOCKET_PROTOCOL_BYTES as usize;
const MAX_WEBSOCKET_MESSAGE_BYTES_USIZE: usize = MAX_WEBSOCKET_MESSAGE_BYTES as usize;
const MAX_WEBSOCKET_CLOSE_REASON_BYTES_USIZE: usize = MAX_WEBSOCKET_CLOSE_REASON_BYTES as usize;
/// Hard cap on total queued inbound WebSocket message payload bytes per socket.
///
/// WebSocket events are delivered to JS by queueing `EventLoop` tasks. A hostile server can send
/// many max-sized messages faster than the JS event loop can process them, causing unbounded memory
/// growth if we only cap by event count.
///
/// When this limit would be exceeded we drop the message and close the connection with code 1009
/// ("Message Too Big") to keep memory usage bounded.
#[cfg(not(test))]
const MAX_WEBSOCKET_PENDING_EVENT_BYTES: usize = 32 * 1024 * 1024;
// Keep byte-cap tests deterministic with a smaller limit (the production cap is large enough to
// tolerate real-world bursts without closing).
#[cfg(test)]
const MAX_WEBSOCKET_PENDING_EVENT_BYTES: usize = 512 * 1024;
const MAX_QUEUED_WEBSOCKET_SEND_COMMANDS: usize = 1_024;
#[cfg(not(test))]
const MAX_QUEUED_WEBSOCKET_EVENTS_PER_SOCKET: usize = 1_024;
// Keep websocket event-queue tests deterministic by forcing a very low cap (the production cap is
// large enough to tolerate real-world bursts).
#[cfg(test)]
const MAX_QUEUED_WEBSOCKET_EVENTS_PER_SOCKET: usize = 4;

// WebSocket close codes:
// - 1009 = "Message Too Big". Used as a "too much data" signal when the renderer cannot keep up
//   with the incoming event stream and would otherwise drop events.
const WS_CLOSE_EVENT_QUEUE_OVERFLOW_CODE: u16 = 1009;
const WS_CLOSE_EVENT_QUEUE_OVERFLOW_REASON: &str = "event queue overflow";
// Defensive cap to prevent untrusted JS from exhausting renderer memory by repeatedly constructing
// WebSockets that leave behind Rust-side state.
const MAX_WEBSOCKETS_PER_ENV: usize = 1_024;

const MAX_QUEUED_DNS_LOOKUPS: usize = 128;

/// Hard timeouts used while establishing a WebSocket connection.
///
/// These are enforced in the I/O thread so a hostile network cannot hang the host indefinitely in
/// DNS/TCP/TLS/WebSocket handshakes.
#[derive(Clone, Copy, Debug)]
pub struct WindowWebSocketTimeouts {
  /// DNS lookup + TCP connect.
  pub dns_tcp_connect: Duration,
  /// TLS handshake (for `wss://`).
  pub tls_handshake: Duration,
  /// WebSocket (HTTP upgrade) handshake.
  pub websocket_handshake: Duration,
}

impl Default for WindowWebSocketTimeouts {
  fn default() -> Self {
    Self {
      // Defaults are intentionally small so renderer teardown can never block for long even when a
      // WebSocket thread is in the middle of connect/handshake.
      #[cfg(not(test))]
      dns_tcp_connect: Duration::from_secs(5),
      #[cfg(test)]
      dns_tcp_connect: Duration::from_secs(1),
      #[cfg(not(test))]
      tls_handshake: Duration::from_secs(5),
      #[cfg(test)]
      tls_handshake: Duration::from_secs(1),
      #[cfg(not(test))]
      websocket_handshake: Duration::from_secs(5),
      #[cfg(test)]
      websocket_handshake: Duration::from_secs(1),
    }
  }
}

#[derive(Clone)]
pub struct WindowWebSocketEnv {
  pub fetcher: Arc<dyn ResourceFetcher>,
  pub document_url: Option<String>,
  pub timeouts: WindowWebSocketTimeouts,
}

impl WindowWebSocketEnv {
  pub fn for_document(fetcher: Arc<dyn ResourceFetcher>, document_url: Option<String>) -> Self {
    Self {
      fetcher,
      document_url,
      timeouts: WindowWebSocketTimeouts::default(),
    }
  }

  pub fn with_timeouts(mut self, timeouts: WindowWebSocketTimeouts) -> Self {
    self.timeouts = timeouts;
    self
  }
}

#[derive(Debug)]
struct IpcNoopFetcher;

impl ResourceFetcher for IpcNoopFetcher {
  fn fetch(&self, url: &str) -> crate::Result<crate::resource::FetchedResource> {
    Err(crate::error::Error::Other(format!(
      "WebSocket IPC backend attempted unexpected in-process fetch for {url}"
    )))
  }
}

struct EnvState {
  env: WindowWebSocketEnv,
  ipc: Option<IpcEnvState>,
  realm_id: RealmId,
  next_id: u64,
  sockets: HashMap<u64, WebSocketState>,
  last_gc_runs: u64,
}

impl EnvState {
  fn new(env: WindowWebSocketEnv, realm_id: RealmId) -> Self {
    Self {
      env,
      ipc: None,
      realm_id,
      next_id: 1,
      sockets: HashMap::new(),
      last_gc_runs: 0,
    }
  }

  fn new_ipc(env: WindowWebSocketEnv, realm_id: RealmId, ipc: IpcEnvState) -> Self {
    Self {
      env,
      ipc: Some(ipc),
      realm_id,
      next_id: 1,
      sockets: HashMap::new(),
      last_gc_runs: 0,
    }
  }

  fn alloc_id(&mut self) -> u64 {
    let id = self.next_id;
    self.next_id = self.next_id.saturating_add(1);
    id
  }
}

struct WebSocketState {
  weak_obj: WeakGcObject,
  url: String,
  requested_protocols: Vec<String>,
  protocol: String,
  ready_state: u16,
  binary_type: WebSocketBinaryType,
  buffered_amount: usize,
  pending_events: usize,
  pending_event_bytes: usize,
  close_task_queued: bool,
  /// When set, the renderer has determined the socket must close (e.g. incoming event queue
  /// overflow). This is used as a fallback close trigger even if the command channel is full.
  forced_close: Option<(u16, String)>,
  // In-process backend (tungstenite) uses a per-socket command queue and thread.
  cmd_tx: Option<mpsc::SyncSender<WsCommand>>,
  thread: Option<JoinHandle<()>>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum WebSocketBinaryType {
  Blob,
  ArrayBuffer,
}

impl Default for WebSocketBinaryType {
  fn default() -> Self {
    // Browser default: Blob.
    Self::Blob
  }
}

impl WebSocketBinaryType {
  fn as_str(self) -> &'static str {
    match self {
      Self::Blob => "blob",
      Self::ArrayBuffer => "arraybuffer",
    }
  }

  fn parse(value: &str) -> Option<Self> {
    match value {
      "blob" => Some(Self::Blob),
      "arraybuffer" => Some(Self::ArrayBuffer),
      _ => None,
    }
  }
}

enum WsCommand {
  SendText(String),
  SendBinary(Vec<u8>),
  Close {
    code: Option<u16>,
    reason: Option<String>,
  },
  Shutdown,
}

/// IPC commands emitted by the renderer/JS realm and consumed by a network process.
///
/// Tests can wire these up to an in-process "network process" thread; production embeddings can
/// forward them across a real IPC boundary.
pub type WebSocketIpcCommand = crate::ipc::RendererToNetwork;

/// IPC events emitted by a network process and consumed by the renderer/JS realm.
pub type WebSocketIpcEvent = crate::ipc::NetworkToRenderer;

// -----------------------------------------------------------------------------
// WebSocket IPC framing (renderer ↔ network process)
// -----------------------------------------------------------------------------
//
// Security note:
// WebSocket commands/events can contain large binary payloads (up to `MAX_WEBSOCKET_MESSAGE_BYTES`).
// The network process must treat the renderer as untrusted: a malicious renderer can otherwise send
// a bogus length prefix that tricks the receiver into allocating unbounded buffers before
// deserialization/validation runs.
//
// We therefore enforce a hard maximum encoded frame size. Frames larger than this should be
// treated as a protocol violation and the renderer IPC channel terminated.

/// Maximum encoded WebSocket IPC frame size (bytes).
///
/// This is intentionally sized above the maximum *allowed* WebSocket message payload
/// (`MAX_WEBSOCKET_MESSAGE_BYTES`, 4 MiB) plus serialization overhead.
pub const MAX_WEBSOCKET_IPC_FRAME_BYTES: usize = crate::ipc::MAX_IPC_MESSAGE_BYTES;

/// Write a renderer→network WebSocket command frame.
pub fn write_websocket_ipc_command_frame<W: Write>(
  writer: &mut W,
  cmd: &WebSocketIpcCommand,
) -> Result<(), IpcError> {
  crate::ipc::framing::write_bincode_frame(writer, cmd)?;
  writer.flush()?;
  Ok(())
}

/// Read a renderer→network WebSocket command frame.
pub fn read_websocket_ipc_command_frame<R: Read>(reader: &mut R) -> Result<WebSocketIpcCommand, IpcError> {
  crate::ipc::framing::read_bincode_frame(reader)
}

/// Write a network→renderer WebSocket event frame.
pub fn write_websocket_ipc_event_frame<W: Write>(
  writer: &mut W,
  event: &WebSocketIpcEvent,
) -> Result<(), IpcError> {
  crate::ipc::framing::write_bincode_frame(writer, event)?;
  writer.flush()?;
  Ok(())
}

/// Read a network→renderer WebSocket event frame.
pub fn read_websocket_ipc_event_frame<R: Read>(reader: &mut R) -> Result<WebSocketIpcEvent, IpcError> {
  crate::ipc::framing::read_bincode_frame(reader)
}

/// Environment configuration for installing the IPC-based WebSocket backend.
///
/// The `cmd_tx` channel should generally be bounded (`sync_channel`) so the renderer can apply
/// backpressure via `WebSocket.send()` throwing once the queue fills.
pub struct WindowWebSocketIpcEnv {
  pub fetcher: Arc<dyn ResourceFetcher>,
  pub document_url: Option<String>,
  pub cmd_tx: mpsc::SyncSender<WebSocketIpcCommand>,
  pub event_rx: mpsc::Receiver<WebSocketIpcEvent>,
}

struct IpcEnvState {
  cmd_tx: mpsc::SyncSender<WebSocketIpcCommand>,
  event_rx: Option<mpsc::Receiver<WebSocketIpcEvent>>,
  stop: Arc<AtomicBool>,
  thread: Option<JoinHandle<()>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum QueueWsTaskOutcome {
  Queued,
  Skipped,
  DeliveryFailed,
}

static NEXT_ENV_ID: AtomicU64 = AtomicU64::new(1);
static ENVS: OnceLock<Mutex<HashMap<u64, EnvState>>> = OnceLock::new();

struct DnsLookupRequest {
  host: String,
  port: u16,
  resp: mpsc::Sender<Result<Vec<SocketAddr>, std::io::Error>>,
}

static DNS_LOOKUP_TX: OnceLock<mpsc::SyncSender<DnsLookupRequest>> = OnceLock::new();

fn dns_lookup_tx() -> mpsc::SyncSender<DnsLookupRequest> {
  DNS_LOOKUP_TX
    .get_or_init(|| {
      // DNS resolution can block inside system resolvers. Run it on a dedicated worker thread so
      // the WebSocket I/O thread can remain responsive to shutdown and enforce a hard connect
      // timeout.
      let (tx, rx) = mpsc::sync_channel::<DnsLookupRequest>(MAX_QUEUED_DNS_LOOKUPS);
      // If the OS refuses to create the worker thread (e.g. thread-limit or memory pressure),
      // avoid panicking. The returned sender will be disconnected (no receiver), and individual
      // connects will fail with a normal error.
      let _ = std::thread::Builder::new()
        .name("websocket-dns".to_string())
        .spawn(move || {
          while let Ok(req) = rx.recv() {
            let result = (req.host.as_str(), req.port)
              .to_socket_addrs()
              .map(|iter| iter.collect::<Vec<_>>());
            let _ = req.resp.send(result);
          }
        });
      tx
    })
    .clone()
}

#[cfg(test)]
static ACTIVE_WEBSOCKET_THREADS: std::sync::atomic::AtomicUsize =
  std::sync::atomic::AtomicUsize::new(0);

#[cfg(test)]
fn active_websocket_threads() -> usize {
  ACTIVE_WEBSOCKET_THREADS.load(Ordering::Relaxed)
}

fn envs() -> &'static Mutex<HashMap<u64, EnvState>> {
  ENVS.get_or_init(|| Mutex::new(HashMap::new()))
}

pub fn unregister_window_websocket_env(env_id: u64) {
  // Remove first so subsequent lookups fail while we join threads.
  let env_state = {
    let mut lock = envs().lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    lock.remove(&env_id)
  };

  let Some(mut env_state) = env_state else {
    return;
  };

  // Best-effort shutdown of any IPC-backed connections. Without this, the network process could keep
  // the conn_id alive after the JS realm is torn down (e.g. navigation within the same renderer
  // process).
  if let Some(ipc) = env_state.ipc.as_ref() {
    for ws_id in env_state.sockets.keys().copied() {
      let _ = ipc.cmd_tx.try_send(WebSocketIpcCommand::WebSocket {
        conn_id: ws_id,
        cmd: WebSocketCommand::Shutdown,
      });
    }
  }

  if let Some(mut ipc) = env_state.ipc.take() {
    ipc.stop.store(true, Ordering::Relaxed);
    if let Some(handle) = ipc.thread.take() {
      let _ = handle.join();
    }
    // Drop remaining rx/tx so embeddings can observe disconnects if desired.
    drop(ipc);
  }

  for (_id, mut ws) in env_state.sockets.drain() {
    // Best-effort shutdown: if the channel is full/disconnected, dropping the sender will still
    // cause the thread to eventually exit.
    if let Some(cmd_tx) = ws.cmd_tx.take() {
      let _ = cmd_tx.try_send(WsCommand::Shutdown);
      // Drop the sender before join so the websocket thread can observe disconnect even if the
      // queue was full and we failed to enqueue `Shutdown`.
      drop(cmd_tx);
    }
    if let Some(handle) = ws.thread.take() {
      let _ = handle.join();
    }
  }
}

#[derive(Debug)]
#[must_use = "websocket bindings are only valid while the returned WindowWebSocketBindings is kept alive"]
pub struct WindowWebSocketBindings {
  env_id: u64,
  active: bool,
}

impl WindowWebSocketBindings {
  fn new(env_id: u64) -> Self {
    Self { env_id, active: true }
  }

  pub fn env_id(&self) -> u64 {
    self.env_id
  }

  fn disarm(mut self) -> u64 {
    self.active = false;
    self.env_id
  }
}

impl Drop for WindowWebSocketBindings {
  fn drop(&mut self) {
    if self.active {
      unregister_window_websocket_env(self.env_id);
    }
  }
}

fn with_env_state<R>(env_id: u64, f: impl FnOnce(&EnvState) -> Result<R, VmError>) -> Result<R, VmError> {
  let lock = envs().lock().unwrap_or_else(|poisoned| poisoned.into_inner());
  let state = lock
    .get(&env_id)
    .ok_or(VmError::Unimplemented("WebSocket env id not registered"))?;
  f(state)
}

fn with_env_state_mut<R>(env_id: u64, f: impl FnOnce(&mut EnvState) -> Result<R, VmError>) -> Result<R, VmError> {
  let mut lock = envs().lock().unwrap_or_else(|poisoned| poisoned.into_inner());
  let state = lock
    .get_mut(&env_id)
    .ok_or(VmError::Unimplemented("WebSocket env id not registered"))?;
  f(state)
}

fn shutdown_ws_state_locked(state: &EnvState, ws_id: u64, ws: WebSocketState) {
  // Best-effort shutdown:
  // - In-process backend: send Shutdown on the per-socket channel.
  // - IPC backend: send a Shutdown request to the network process.
  if let Some(ipc) = state.ipc.as_ref() {
    let _ = ipc.cmd_tx.try_send(WebSocketIpcCommand::WebSocket {
      conn_id: ws_id,
      cmd: WebSocketCommand::Shutdown,
    });
  }
  if let Some(cmd_tx) = ws.cmd_tx.as_ref() {
    let _ = cmd_tx.try_send(WsCommand::Shutdown);
  }
  // Dropping the JoinHandle detaches the thread; OS resources are reclaimed once it exits.
  drop(ws);
}

fn sweep_env_state_if_gc_ran_locked(state: &mut EnvState, heap: &Heap) {
  let gc_runs = heap.gc_runs();
  if gc_runs == state.last_gc_runs {
    return;
  }
  state.last_gc_runs = gc_runs;

  // Drop Rust-side state for sockets whose JS wrapper is no longer reachable.
  //
  // The `WeakGcObject` only stops upgrading after a heap GC, so we only sweep when `gc_runs`
  // changes to avoid O(n) scans on every accessor call.
  let mut dead_ws_ids: Vec<u64> = Vec::new();
  for (&ws_id, ws) in state.sockets.iter() {
    if ws.weak_obj.upgrade(heap).is_none() {
      dead_ws_ids.push(ws_id);
    }
  }
  for ws_id in dead_ws_ids {
    if let Some(ws) = state.sockets.remove(&ws_id) {
      shutdown_ws_state_locked(state, ws_id, ws);
    }
  }
}

fn sweep_env_state_if_gc_ran(env_id: u64, heap: &Heap) -> Result<(), VmError> {
  let mut lock = envs().lock().unwrap_or_else(|poisoned| poisoned.into_inner());
  let state = lock
    .get_mut(&env_id)
    .ok_or(VmError::Unimplemented("WebSocket env id not registered"))?;
  sweep_env_state_if_gc_ran_locked(state, heap);
  Ok(())
}

fn data_desc(value: Value, writable: bool) -> PropertyDescriptor {
  PropertyDescriptor {
    enumerable: false,
    configurable: true,
    kind: PropertyKind::Data { value, writable },
  }
}

fn accessor_desc(get: Value, set: Value) -> PropertyDescriptor {
  PropertyDescriptor {
    enumerable: false,
    configurable: true,
    kind: PropertyKind::Accessor { get, set },
  }
}

fn alloc_key(scope: &mut Scope<'_>, name: &str) -> Result<PropertyKey, VmError> {
  let s = scope.alloc_string(name)?;
  scope.push_root(Value::String(s))?;
  Ok(PropertyKey::from_string(s))
}

fn get_data_prop(scope: &mut Scope<'_>, obj: GcObject, name: &str) -> Result<Value, VmError> {
  let key = alloc_key(scope, name)?;
  Ok(
    scope
      .heap()
      .object_get_own_data_property_value(obj, &key)?
      .unwrap_or(Value::Undefined),
  )
}

fn set_data_prop(
  scope: &mut Scope<'_>,
  obj: GcObject,
  name: &str,
  value: Value,
  writable: bool,
) -> Result<(), VmError> {
  let mut scope = scope.reborrow();
  scope.push_root(Value::Object(obj))?;
  scope.push_root(value)?;
  let key = alloc_key(&mut scope, name)?;
  scope.define_property(obj, key, data_desc(value, writable))
}

fn set_internal_prop(scope: &mut Scope<'_>, obj: GcObject, name: &str, value: Value) -> Result<(), VmError> {
  let mut scope = scope.reborrow();
  scope.push_root(Value::Object(obj))?;
  scope.push_root(value)?;
  let key = alloc_key(&mut scope, name)?;
  scope.define_property(
    obj,
    key,
    PropertyDescriptor {
      enumerable: false,
      configurable: false,
      kind: PropertyKind::Data {
        value,
        writable: false,
      },
    },
  )
}

fn set_ws_tombstone_props(
  scope: &mut Scope<'_>,
  ws_obj: GcObject,
  url: &str,
  protocol: &str,
  ready_state: u16,
  buffered_amount: usize,
) -> Result<(), VmError> {
  // Idempotent: if the tombstone keys already exist (e.g. if cleanup runs twice), skip.
  if !matches!(get_data_prop(scope, ws_obj, WS_TOMBSTONE_READY_STATE_KEY)?, Value::Undefined) {
    return Ok(());
  }

  let url_s = scope.alloc_string(url)?;
  set_internal_prop(scope, ws_obj, WS_TOMBSTONE_URL_KEY, Value::String(url_s))?;
  let protocol_s = scope.alloc_string(protocol)?;
  set_internal_prop(
    scope,
    ws_obj,
    WS_TOMBSTONE_PROTOCOL_KEY,
    Value::String(protocol_s),
  )?;
  set_internal_prop(
    scope,
    ws_obj,
    WS_TOMBSTONE_READY_STATE_KEY,
    Value::Number(ready_state as f64),
  )?;
  set_internal_prop(
    scope,
    ws_obj,
    WS_TOMBSTONE_BUFFERED_AMOUNT_KEY,
    Value::Number(buffered_amount as f64),
  )?;
  Ok(())
}

fn env_id_from_callee(scope: &mut Scope<'_>, callee: GcObject) -> Result<u64, VmError> {
  let slots = scope.heap().get_function_native_slots(callee)?;
  let value = slots.get(SLOT_ENV_ID).copied().unwrap_or(Value::Undefined);
  let Value::Number(n) = value else {
    return Err(VmError::InvariantViolation(
      "WebSocket constructor missing env id slot",
    ));
  };
  if !n.is_finite() || n < 0.0 || n > u64::MAX as f64 {
    return Err(VmError::InvariantViolation(
      "WebSocket constructor env id slot invalid",
    ));
  }
  Ok(n as u64)
}

fn parse_env_and_ws_id(scope: &mut Scope<'_>, this: Value) -> Result<(u64, u64, GcObject), VmError> {
  let Value::Object(obj) = this else {
    return Err(VmError::TypeError("Illegal invocation"));
  };
  if !scope.heap().is_valid_object(obj) {
    return Err(VmError::TypeError("Illegal invocation"));
  }
  let Some(slots) = scope.heap().object_host_slots(obj)? else {
    return Err(VmError::TypeError("Illegal invocation"));
  };
  if slots.a != WEBSOCKET_HOST_TAG {
    return Err(VmError::TypeError("Illegal invocation"));
  }

  scope.push_root(Value::Object(obj))?;
  let env_key = alloc_key(scope, ENV_ID_KEY)?;
  let ws_key = alloc_key(scope, WS_ID_KEY)?;

  let env = scope
    .heap()
    .object_get_own_data_property_value(obj, &env_key)?
    .unwrap_or(Value::Undefined);
  let ws = scope
    .heap()
    .object_get_own_data_property_value(obj, &ws_key)?
    .unwrap_or(Value::Undefined);

  let env_id = match env {
    Value::Number(n) if n.is_finite() && n >= 0.0 && n <= u64::MAX as f64 => n as u64,
    _ => return Err(VmError::TypeError("Illegal invocation")),
  };
  let ws_id = match ws {
    Value::Number(n) if n.is_finite() && n >= 0.0 && n <= u64::MAX as f64 => n as u64,
    _ => return Err(VmError::TypeError("Illegal invocation")),
  };

  Ok((env_id, ws_id, obj))
}

fn js_string_to_rust_string_limited(
  heap: &Heap,
  handle: GcString,
  max_bytes: usize,
  error: &'static str,
) -> Result<String, VmError> {
  let js = heap.get_string(handle)?;
  let code_units_len = js.len_code_units();
  if code_units_len > max_bytes {
    return Err(VmError::TypeError(error));
  }

  let capacity = code_units_len.saturating_mul(3).min(max_bytes);
  let mut out = String::with_capacity(capacity);
  let mut out_len = 0usize;
  for decoded in decode_utf16(js.as_code_units().iter().copied()) {
    let ch = decoded.unwrap_or('\u{FFFD}');
    let ch_len = ch.len_utf8();
    let next_len = out_len.checked_add(ch_len).unwrap_or(usize::MAX);
    if next_len > max_bytes {
      return Err(VmError::TypeError(error));
    }
    out.push(ch);
    out_len = next_len;
  }
  Ok(out)
}

fn to_rust_string_limited(
  heap: &mut Heap,
  value: Value,
  max_bytes: usize,
  error: &'static str,
) -> Result<String, VmError> {
  let s = heap.to_string(value)?;
  js_string_to_rust_string_limited(heap, s, max_bytes, error)
}

fn current_document_base_url(vm: &mut Vm, env_id: u64) -> Result<Option<String>, VmError> {
  if let Some(data) = vm.user_data_mut::<WindowRealmUserData>() {
    return Ok(data.base_url.clone());
  }
  with_env_state(env_id, |state| Ok(state.env.document_url.clone()))
}

fn current_document_is_secure_context(vm: &mut Vm, env_id: u64) -> bool {
  if let Some(data) = vm.user_data::<WindowRealmUserData>() {
    return is_secure_context_for_document_url(data.document_url());
  }
  with_env_state(env_id, |state| Ok(state.env.document_url.clone()))
    .ok()
    .flatten()
    .map(|url| is_secure_context_for_document_url(&url))
    .unwrap_or(false)
}

fn resolve_websocket_url(vm: &mut Vm, env_id: u64, url: &str) -> Result<url::Url, VmError> {
  let base = current_document_base_url(vm, env_id)?;
  let resolved_href = resolve_url(url, base.as_deref()).map_err(|err| match err {
    UrlResolveError::RelativeUrlWithoutBase => VmError::TypeError(
      "WebSocket URL is relative, but the current document has no base URL",
    ),
    UrlResolveError::Url(_) => VmError::TypeError("WebSocket URL is invalid"),
  })?;

  let mut resolved =
    url::Url::parse(&resolved_href).map_err(|_| VmError::TypeError("WebSocket URL is invalid"))?;

  if resolved.fragment().is_some() {
    return Err(VmError::TypeError(
      "WebSocket URL must not include a fragment",
    ));
  }

  match resolved.scheme() {
    "ws" | "wss" => {}
    "http" => {
      let _ = resolved.set_scheme("ws");
    }
    "https" => {
      let _ = resolved.set_scheme("wss");
    }
    _ => {
      return Err(VmError::TypeError(
        "WebSocket URL must use ws: or wss: scheme",
      ))
    }
  }

  Ok(resolved)
}

fn is_valid_websocket_subprotocol_token(s: &str) -> bool {
  if s.is_empty() {
    return false;
  }
  s.bytes().all(|b| {
    matches!(b, b'0'..=b'9' | b'a'..=b'z' | b'A'..=b'Z')
      || matches!(
        b,
        b'!' | b'#' | b'$' | b'%' | b'&' | b'\'' | b'*' | b'+' | b'-' | b'.' | b'^' | b'_' | b'`' | b'|' | b'~'
      )
  })
}

fn parse_protocols(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  value: Value,
) -> Result<Vec<String>, VmError> {
  if matches!(value, Value::Undefined | Value::Null) {
    return Ok(Vec::new());
  }

  match value {
    Value::String(s) => {
      let s = js_string_to_rust_string_limited(
        scope.heap(),
        s,
        MAX_WEBSOCKET_PROTOCOL_BYTES_USIZE,
        "WebSocket protocol too long",
      )?;
      if s.is_empty() {
        return Err(VmError::TypeError("WebSocket protocol must not be empty"));
      }
      if !is_valid_websocket_subprotocol_token(&s) {
        return Err(VmError::TypeError("WebSocket protocol must be a token"));
      }
      Ok(vec![s])
    }
    Value::Object(_) => {
      let iterable = value;
      let mut record = iterator::get_iterator(vm, host, hooks, scope, iterable)?;
      let mut out: Vec<String> = Vec::new();
      let mut count: usize = 0;
      let collect_result: Result<(), VmError> = (|| {
        loop {
          vm.tick()?;
          let Some(item) = iterator::iterator_step_value(vm, host, hooks, scope, &mut record)? else {
            break;
          };
          count += 1;
          if count > MAX_WEBSOCKET_PROTOCOLS_USIZE {
            return Err(VmError::TypeError("WebSocket protocols list is too large"));
          }
          let s = to_rust_string_limited(
            scope.heap_mut(),
            item,
            MAX_WEBSOCKET_PROTOCOL_BYTES_USIZE,
            "WebSocket protocol too long",
          )?;
          if s.is_empty() {
            return Err(VmError::TypeError("WebSocket protocol must not be empty"));
          }
          if !is_valid_websocket_subprotocol_token(&s) {
            return Err(VmError::TypeError("WebSocket protocol must be a token"));
          }
          if out.iter().any(|p| p == &s) {
            return Err(VmError::TypeError(
              "WebSocket protocols list must not contain duplicates",
            ));
          }
          out.push(s);
        }
        Ok(())
      })();

      if let Err(err) = collect_result {
        if !record.done {
          // `iterator_close` can allocate / run user code (and thus GC). If we're returning a thrown
          // value, root it so the handle stays valid until we bubble it up.
          let original_is_throw = err.is_throw_completion();
          let pending_root = err
            .thrown_value()
            .map(|v| scope.heap_mut().add_root(v))
            .transpose()?;
          let close_res =
            iterator::iterator_close(vm, host, hooks, scope, &record, CloseCompletionKind::Throw);
          if let Some(root) = pending_root {
            scope.heap_mut().remove_root(root);
          }
          // IteratorClose errors override non-throw completions, but must never suppress fatal VM
          // errors (termination/OOM). `CloseCompletionKind::Throw` already suppresses throw
          // completions from `return`; if we still get an error here, treat it as fatal.
          if let Err(close_err) = close_res {
            if original_is_throw {
              return Err(close_err);
            }
          }
        }
        return Err(err);
      }
      if !record.done {
        iterator::iterator_close(vm, host, hooks, scope, &record, CloseCompletionKind::NonThrow)?;
      }
      Ok(out)
    }
    other => {
      // Union conversion fallback: treat as DOMString.
      let s = to_rust_string_limited(
        scope.heap_mut(),
        other,
        MAX_WEBSOCKET_PROTOCOL_BYTES_USIZE,
        "WebSocket protocol too long",
      )?;
      if s.is_empty() {
        return Err(VmError::TypeError("WebSocket protocol must not be empty"));
      }
      if !is_valid_websocket_subprotocol_token(&s) {
        return Err(VmError::TypeError("WebSocket protocol must be a token"));
      }
      Ok(vec![s])
    }
  }
}

fn websocket_ctor_call(
  _vm: &mut Vm,
  _scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  Err(VmError::TypeError(
    "WebSocket constructor cannot be invoked without 'new'",
  ))
}

fn websocket_ctor_construct<Host: WindowRealmHost + 'static>(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  args: &[Value],
  new_target: Value,
) -> Result<Value, VmError> {
  let env_id = env_id_from_callee(scope, callee)?;

  let url_value = args.get(0).copied().unwrap_or(Value::Undefined);
  if matches!(url_value, Value::Undefined) {
    return Err(VmError::TypeError("WebSocket URL is required"));
  }
  let url_str = to_rust_string_limited(
    scope.heap_mut(),
    url_value,
    MAX_WEBSOCKET_URL_BYTES_USIZE,
    "WebSocket URL exceeds maximum length",
  )?;

  let resolved_url = resolve_websocket_url(vm, env_id, &url_str).map_err(|err| match err {
    VmError::TypeError(_) => err,
    other => other,
  })?;

  let document_is_secure = current_document_is_secure_context(vm, env_id);
  if document_is_secure && resolved_url.scheme() == "ws" {
    return Err(VmError::TypeError(
      "Mixed content is blocked: cannot connect to ws: from a secure context",
    ));
  }

  let protocols_value = args.get(1).copied().unwrap_or(Value::Undefined);
  let protocols = parse_protocols(vm, scope, host, hooks, protocols_value)?;

  let Some(event_loop) = event_loop_mut_from_hooks::<Host>(hooks) else {
    return Err(VmError::TypeError(
      "WebSocket constructed without an active EventLoop",
    ));
  };
  let task_queue: ExternalTaskQueueHandle<Host> = event_loop.external_task_queue_handle();

  // Sweep unreachable sockets (after GC) and enforce a hard per-env cap before allocating the JS
  // wrapper / spawning the I/O thread.
  {
    let heap = scope.heap();
    with_env_state_mut(env_id, |state| {
      sweep_env_state_if_gc_ran_locked(state, heap);
      if state.sockets.len() >= MAX_WEBSOCKETS_PER_ENV {
        return Err(VmError::TypeError("Too many WebSocket connections"));
      }
      Ok(())
    })?;
  }

  // Create the JS wrapper object with `newTarget.prototype`.
  let ctor = match new_target {
    Value::Object(obj) => obj,
    _ => callee,
  };
  let prototype_key = alloc_key(scope, "prototype")?;
  let proto = scope
    .heap()
    .object_get_own_data_property_value(ctor, &prototype_key)?
    .and_then(|v| match v {
      Value::Object(obj) => Some(obj),
      _ => None,
    });

  let obj = scope.alloc_object()?;
  scope.push_root(Value::Object(obj))?;
  if let Some(proto) = proto {
    scope.heap_mut().object_set_prototype(obj, Some(proto))?;
  }
  // Brand the wrapper as both a WebSocket (slot `a`) and an EventTarget (slot `b`).
  scope.heap_mut().object_set_host_slots(
    obj,
    HostSlots {
      a: WEBSOCKET_HOST_TAG,
      b: EVENT_TARGET_HOST_TAG,
    },
  )?;

  // Hidden ids.
  let env_key = alloc_key(scope, ENV_ID_KEY)?;
  scope.define_property(
    obj,
    env_key,
    PropertyDescriptor {
      enumerable: false,
      configurable: false,
      kind: PropertyKind::Data {
        value: Value::Number(env_id as f64),
        writable: false,
      },
    },
  )?;

  // Default handler attributes.
  set_data_prop(scope, obj, "onopen", Value::Null, true)?;
  set_data_prop(scope, obj, "onmessage", Value::Null, true)?;
  set_data_prop(scope, obj, "onerror", Value::Null, true)?;
  set_data_prop(scope, obj, "onclose", Value::Null, true)?;

  let use_ipc = with_env_state(env_id, |state| Ok(state.ipc.is_some()))?;

  let (ws_id, cmd_rx_opt) = if use_ipc {
    let ws_id = with_env_state_mut(env_id, |state| {
      let id = state.alloc_id();
      state.sockets.insert(
        id,
        WebSocketState {
          weak_obj: WeakGcObject::from(obj),
          url: resolved_url.to_string(),
          requested_protocols: protocols.clone(),
          protocol: String::new(),
          ready_state: WS_CONNECTING,
          binary_type: WebSocketBinaryType::default(),
          buffered_amount: 0,
          pending_events: 0,
          pending_event_bytes: 0,
          close_task_queued: false,
          forced_close: None,
          cmd_tx: None,
          thread: None,
        },
      );
      Ok(id)
    })?;
    (ws_id, None)
  } else {
    let (cmd_tx, cmd_rx) = mpsc::sync_channel::<WsCommand>(MAX_QUEUED_WEBSOCKET_SEND_COMMANDS);
    let ws_id = with_env_state_mut(env_id, |state| {
      let id = state.alloc_id();
      state.sockets.insert(
        id,
        WebSocketState {
          weak_obj: WeakGcObject::from(obj),
          url: resolved_url.to_string(),
          requested_protocols: protocols.clone(),
          protocol: String::new(),
          ready_state: WS_CONNECTING,
          binary_type: WebSocketBinaryType::default(),
          buffered_amount: 0,
          pending_events: 0,
          pending_event_bytes: 0,
          close_task_queued: false,
          forced_close: None,
          cmd_tx: Some(cmd_tx.clone()),
          thread: None,
        },
      );
      Ok(id)
    })?;
    (ws_id, Some(cmd_rx))
  };

  let ws_key = alloc_key(scope, WS_ID_KEY)?;
  scope.define_property(
    obj,
    ws_key,
    PropertyDescriptor {
      enumerable: false,
      configurable: false,
      kind: PropertyKind::Data {
        value: Value::Number(ws_id as f64),
        writable: false,
      },
    },
  )?;

  let resolved_url_string = resolved_url.to_string();

  if use_ipc {
    ensure_ipc_event_thread_started::<Host>(env_id, task_queue)?;
    let ipc_tx = with_env_state(env_id, |state| {
      state
        .ipc
        .as_ref()
        .map(|ipc| ipc.cmd_tx.clone())
        .ok_or(VmError::InvariantViolation("WebSocket IPC env missing cmd_tx"))
    })?;
    let document_url = with_env_state(env_id, |state| Ok(state.env.document_url.clone()))?;
    let params = WebSocketConnectParams {
      url: resolved_url_string,
      protocols,
      origin: None,
      document_url,
    };
    let msg = WebSocketIpcCommand::WebSocket {
      conn_id: ws_id,
      cmd: WebSocketCommand::Connect { params },
    };
    match ipc_tx.try_send(msg) {
      Ok(()) => {}
      Err(mpsc::TrySendError::Full(_)) => {
        let _ = with_env_state_mut(env_id, |state| {
          state.sockets.remove(&ws_id);
          Ok(())
        });
        return Err(VmError::TypeError("WebSocket connect queue is full"));
      }
      Err(mpsc::TrySendError::Disconnected(_)) => {
        let _ = with_env_state_mut(env_id, |state| {
          state.sockets.remove(&ws_id);
          Ok(())
        });
        return Err(VmError::TypeError("WebSocket is closed"));
      }
    }
  } else if let Some(cmd_rx) = cmd_rx_opt {
    let fetcher = with_env_state(env_id, |state| Ok(Arc::clone(&state.env.fetcher)))?;
    let thread_task_queue = task_queue.clone();
    let spawn_result = std::thread::Builder::new()
      .name(format!("ws-{env_id}-{ws_id}"))
      .spawn(move || {
        websocket_thread_main::<Host>(
          env_id,
          ws_id,
          fetcher,
          resolved_url_string,
          document_is_secure,
          protocols,
          cmd_rx,
          thread_task_queue,
        )
      });

    let handle = match spawn_result {
      Ok(handle) => Some(handle),
      Err(_err) => {
        // If the OS refuses to create a thread (resource exhaustion), treat it as a connection
        // failure: mark the socket closed and emit `error` + `close` events.
        let url_snapshot = with_env_state(env_id, |state| {
          Ok(
            state
              .sockets
              .get(&ws_id)
              .map(|ws| ws.url.clone())
              .unwrap_or_default(),
          )
        })
        .unwrap_or_default();

        let _ = with_env_state_mut(env_id, |state| {
          if let Some(ws) = state.sockets.get_mut(&ws_id) {
            ws.ready_state = WS_CLOSED;
            ws.buffered_amount = 0;
            // Drop the sender so `send()` calls fail immediately.
            ws.cmd_tx = None;
            ws.thread = None;
          }
          Ok(())
        });

        queue_ws_task::<Host>(
          &task_queue,
          env_id,
          ws_id,
          WsTaskKind::Close,
          0,
          move |vm_host, heap, vm, hooks, ws_obj| {
            let mut scope = heap.scope();
            let ev = make_simple_event(&mut scope, "error")?;
            dispatch_ws_event(vm, &mut scope, vm_host, hooks, ws_obj, Value::Object(ev), "onerror")?;
            let close_ev = make_simple_event(&mut scope, "close")?;
            dispatch_ws_event(vm, &mut scope, vm_host, hooks, ws_obj, Value::Object(close_ev), "onclose")?;

            let _ = set_ws_tombstone_props(&mut scope, ws_obj, &url_snapshot, "", WS_CLOSED, 0);
            let _ = with_env_state_mut(env_id, |state| Ok(state.sockets.remove(&ws_id)));
            Ok(())
          },
        );

        None
      }
    };

    let _ = with_env_state_mut(env_id, |state| {
      if let Some(ws) = state.sockets.get_mut(&ws_id) {
        ws.thread = handle;
      }
      Ok(())
    });
  }

  Ok(Value::Object(obj))
}

fn websocket_ready_state_get(
  _vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let (env_id, ws_id, obj) = parse_env_and_ws_id(scope, this)?;
  let _ = sweep_env_state_if_gc_ran(env_id, scope.heap());
  if let Ok(value) = with_env_state(env_id, |state| {
    let ws = state
      .sockets
      .get(&ws_id)
      .ok_or(VmError::TypeError("WebSocket is not initialized"))?;
    Ok(Value::Number(ws.ready_state as f64))
  }) {
    return Ok(value);
  }

  // Fallback to tombstone values after the backing socket entry is removed.
  match get_data_prop(scope, obj, WS_TOMBSTONE_READY_STATE_KEY)? {
    Value::Number(n) => Ok(Value::Number(n)),
    _ => Ok(Value::Number(WS_CLOSED as f64)),
  }
}

fn websocket_url_get(
  _vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let (env_id, ws_id, obj) = parse_env_and_ws_id(scope, this)?;
  let _ = sweep_env_state_if_gc_ran(env_id, scope.heap());
  if let Ok(value) = with_env_state(env_id, |state| {
    let ws = state
      .sockets
      .get(&ws_id)
      .ok_or(VmError::TypeError("WebSocket is not initialized"))?;
    let s = scope.alloc_string(&ws.url)?;
    scope.push_root(Value::String(s))?;
    Ok(Value::String(s))
  }) {
    return Ok(value);
  }

  match get_data_prop(scope, obj, WS_TOMBSTONE_URL_KEY)? {
    Value::String(s) => {
      scope.push_root(Value::String(s))?;
      Ok(Value::String(s))
    }
    _ => {
      let s = scope.alloc_string("")?;
      scope.push_root(Value::String(s))?;
      Ok(Value::String(s))
    }
  }
}

fn websocket_protocol_get(
  _vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let (env_id, ws_id, obj) = parse_env_and_ws_id(scope, this)?;
  let _ = sweep_env_state_if_gc_ran(env_id, scope.heap());
  if let Ok(value) = with_env_state(env_id, |state| {
    let ws = state
      .sockets
      .get(&ws_id)
      .ok_or(VmError::TypeError("WebSocket is not initialized"))?;
    let s = scope.alloc_string(&ws.protocol)?;
    scope.push_root(Value::String(s))?;
    Ok(Value::String(s))
  }) {
    return Ok(value);
  }

  match get_data_prop(scope, obj, WS_TOMBSTONE_PROTOCOL_KEY)? {
    Value::String(s) => {
      scope.push_root(Value::String(s))?;
      Ok(Value::String(s))
    }
    _ => {
      let s = scope.alloc_string("")?;
      scope.push_root(Value::String(s))?;
      Ok(Value::String(s))
    }
  }
}

fn websocket_buffered_amount_get(
  _vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let (env_id, ws_id, obj) = parse_env_and_ws_id(scope, this)?;
  let _ = sweep_env_state_if_gc_ran(env_id, scope.heap());
  if let Ok(value) = with_env_state(env_id, |state| {
    let ws = state
      .sockets
      .get(&ws_id)
      .ok_or(VmError::TypeError("WebSocket is not initialized"))?;
    Ok(Value::Number(ws.buffered_amount as f64))
  }) {
    return Ok(value);
  }

  match get_data_prop(scope, obj, WS_TOMBSTONE_BUFFERED_AMOUNT_KEY)? {
    Value::Number(n) => Ok(Value::Number(n)),
    _ => Ok(Value::Number(0.0)),
  }
}

fn websocket_binary_type_get(
  _vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let (env_id, ws_id, _obj) = parse_env_and_ws_id(scope, this)?;
  with_env_state(env_id, |state| {
    let ws = state
      .sockets
      .get(&ws_id)
      .ok_or(VmError::TypeError("WebSocket is not initialized"))?;
    let s = scope.alloc_string(ws.binary_type.as_str())?;
    scope.push_root(Value::String(s))?;
    Ok(Value::String(s))
  })
}

fn websocket_binary_type_set(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let (env_id, ws_id, _obj) = parse_env_and_ws_id(scope, this)?;
  let value = args.get(0).copied().unwrap_or(Value::Undefined);

  // WebSocket.binaryType is a (limited) enum-like string; this binding uses the VM's minimal
  // `ToString` implementation to stay deterministic (and thus rejects objects).
  let s = scope.heap_mut().to_string(value)?;
  let s = scope.heap().get_string(s)?.to_utf8_lossy();

  let Some(kind) = WebSocketBinaryType::parse(&s) else {
    let intr = vm
      .intrinsics()
      .ok_or(VmError::Unimplemented("WebSocket.binaryType requires intrinsics"))?;
    let value = vm_js::new_syntax_error_object(
      scope,
      &intr,
      "WebSocket.binaryType must be either 'blob' or 'arraybuffer'",
    )?;
    return Err(VmError::Throw(value));
  };

  with_env_state_mut(env_id, |state| {
    let ws = state
      .sockets
      .get_mut(&ws_id)
      .ok_or(VmError::TypeError("WebSocket is not initialized"))?;
    ws.binary_type = kind;
    Ok(())
  })?;

  Ok(Value::Undefined)
}

fn websocket_send<Host: WindowRealmHost + 'static>(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let (env_id, ws_id, _obj) = parse_env_and_ws_id(scope, this)?;
  let _ = sweep_env_state_if_gc_ran(env_id, scope.heap());
  let data = args.get(0).copied().unwrap_or(Value::Undefined);
  if matches!(data, Value::Undefined) {
    return Err(VmError::TypeError("WebSocket.send requires an argument"));
  }

  let mut kind: Option<WsCommand> = None;
  let mut byte_len: usize = 0;

  match data {
    Value::Object(obj) if scope.heap().is_array_buffer_object(obj) => {
      let bytes = scope.heap().array_buffer_data(obj)?;
      byte_len = bytes.len();
      // Avoid cloning potentially-large attacker-controlled buffers before enforcing the message
      // size limit.
      if byte_len > MAX_WEBSOCKET_MESSAGE_BYTES_USIZE {
        return Err(VmError::TypeError("WebSocket message too large"));
      }
      kind = Some(WsCommand::SendBinary(bytes.to_vec()));
    }
    Value::Object(obj) if scope.heap().is_uint8_array_object(obj) => {
      let bytes = scope.heap().uint8_array_data(obj)?;
      byte_len = bytes.len();
      if byte_len > MAX_WEBSOCKET_MESSAGE_BYTES_USIZE {
        return Err(VmError::TypeError("WebSocket message too large"));
      }
      kind = Some(WsCommand::SendBinary(bytes.to_vec()));
    }
    other => {
      if let Some(blob) = window_blob::clone_blob_data_for_fetch(vm, scope.heap(), other)? {
        byte_len = blob.bytes.len();
        kind = Some(WsCommand::SendBinary(blob.bytes));
      } else {
        let s = to_rust_string_limited(
          scope.heap_mut(),
          other,
          MAX_WEBSOCKET_MESSAGE_BYTES_USIZE,
          "WebSocket message too large",
        )?;
        byte_len = s.as_bytes().len();
        kind = Some(WsCommand::SendText(s));
      }
    }
  }

  if byte_len > MAX_WEBSOCKET_MESSAGE_BYTES_USIZE {
    return Err(VmError::TypeError("WebSocket message too large"));
  }

  let cmd = kind.ok_or(VmError::TypeError("WebSocket data unsupported"))?;

  enum SendQueue {
    InProcess(mpsc::SyncSender<WsCommand>),
    Ipc(mpsc::SyncSender<WebSocketIpcCommand>),
  }

  let queue = match with_env_state_mut(env_id, |state| {
    let ws = state
      .sockets
      .get_mut(&ws_id)
      .ok_or(VmError::TypeError("WebSocket is not initialized"))?;
    if ws.ready_state != WS_OPEN {
      return Err(VmError::TypeError("WebSocket is not open"));
    }
    let next_buffered = ws.buffered_amount.saturating_add(byte_len);
    if next_buffered > MAX_WEBSOCKET_BUFFERED_AMOUNT_BYTES {
      return Err(VmError::TypeError(
        "WebSocket bufferedAmount limit exceeded",
      ));
    }
    ws.buffered_amount = next_buffered;
    if let Some(ipc) = state.ipc.as_ref() {
      Ok(SendQueue::Ipc(ipc.cmd_tx.clone()))
    } else {
      let cmd_tx = ws
        .cmd_tx
        .as_ref()
        .cloned()
        .ok_or(VmError::InvariantViolation(
          "WebSocket in-process state missing cmd_tx",
        ))?;
      Ok(SendQueue::InProcess(cmd_tx))
    }
  }) {
    Ok(queue) => queue,
    Err(err) => match err {
      // If the backing socket entry has already been cleaned up, treat it as closed.
      VmError::TypeError(_) | VmError::Unimplemented(_) => {
        return Err(VmError::TypeError("WebSocket is not open"))
      }
      other => return Err(other),
    },
  };

  let revert_buffered = |byte_len: usize| {
    let _ = with_env_state_mut(env_id, |state| {
      if let Some(ws) = state.sockets.get_mut(&ws_id) {
        ws.buffered_amount = ws.buffered_amount.saturating_sub(byte_len);
      }
      Ok(())
    });
  };

  match queue {
    SendQueue::InProcess(cmd_tx) => match cmd_tx.try_send(cmd) {
      Ok(()) => Ok(Value::Undefined),
      Err(mpsc::TrySendError::Full(_)) => {
        revert_buffered(byte_len);
        Err(VmError::TypeError("WebSocket send queue is full"))
      }
      Err(mpsc::TrySendError::Disconnected(_)) => {
        revert_buffered(byte_len);
        Err(VmError::TypeError("WebSocket is closed"))
      }
    },
    SendQueue::Ipc(cmd_tx) => {
      let cmd = match cmd {
        WsCommand::SendText(text) => WebSocketCommand::SendText { text },
        WsCommand::SendBinary(data) => WebSocketCommand::SendBinary { data },
        _ => unreachable!("websocket_send only queues SendText/SendBinary"),
      };
      let msg = WebSocketIpcCommand::WebSocket { conn_id: ws_id, cmd };
      match cmd_tx.try_send(msg) {
        Ok(()) => Ok(Value::Undefined),
        Err(mpsc::TrySendError::Full(_)) => {
          revert_buffered(byte_len);
          Err(VmError::TypeError("WebSocket send queue is full"))
        }
        Err(mpsc::TrySendError::Disconnected(_)) => {
          revert_buffered(byte_len);
          Err(VmError::TypeError("WebSocket is closed"))
        }
      }
    }
  }
}

fn is_valid_websocket_close_code(code: u16) -> bool {
  // WHATWG WebSocket: `close(code, reason)` only allows status codes 1000 or 3000–4999.
  // This rejects reserved/illegal codes such as 1004–1006 and 1015, and ensures we never send
  // them on the wire.
  code == 1000 || (3000..=4999).contains(&code)
}

fn websocket_close<Host: WindowRealmHost + 'static>(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let (env_id, ws_id, _obj) = parse_env_and_ws_id(scope, this)?;
  let _ = sweep_env_state_if_gc_ran(env_id, scope.heap());

  let code_value = args.get(0).copied().unwrap_or(Value::Undefined);
  let reason_value = args.get(1).copied().unwrap_or(Value::Undefined);

  let code: Option<u16> = if matches!(code_value, Value::Undefined | Value::Null) {
    None
  } else {
    // Unlike WebIDL's `unsigned short` conversion (`ToUint16`), do not wrap values modulo 2^16.
    // Treat NaN/±Infinity/negative/out-of-range as invalid so we don't send reserved/illegal close
    // codes due to wrapping.
    let n = scope.to_number(vm, host, hooks, code_value)?;
    if !n.is_finite() {
      return Err(VmError::TypeError("WebSocket close code is invalid"));
    }
    let n = n.trunc();
    if n < 0.0 || n > 65535.0 {
      return Err(VmError::TypeError("WebSocket close code is invalid"));
    }
    let code = n as u16;
    if !is_valid_websocket_close_code(code) {
      return Err(VmError::TypeError("WebSocket close code is invalid"));
    }
    Some(code)
  };

  let reason: Option<String> = if matches!(reason_value, Value::Undefined | Value::Null) {
    None
  } else {
    let s = to_rust_string_limited(
      scope.heap_mut(),
      reason_value,
      MAX_WEBSOCKET_CLOSE_REASON_BYTES_USIZE,
      "WebSocket close reason too long",
    )?;
    Some(s)
  };

  enum CloseQueue {
    InProcess(mpsc::SyncSender<WsCommand>),
    Ipc(mpsc::SyncSender<WebSocketIpcCommand>),
  }

  let queue: Option<CloseQueue> = match with_env_state_mut(env_id, |state| {
    let Some(ws) = state.sockets.get_mut(&ws_id) else {
      return Ok(None);
    };
    if ws.ready_state == WS_CLOSING || ws.ready_state == WS_CLOSED {
      return Ok(None);
    }
    ws.ready_state = WS_CLOSING;
    if let Some(ipc) = state.ipc.as_ref() {
      Ok(Some(CloseQueue::Ipc(ipc.cmd_tx.clone())))
    } else {
      let cmd_tx = ws
        .cmd_tx
        .as_ref()
        .cloned()
        .ok_or(VmError::InvariantViolation(
          "WebSocket in-process state missing cmd_tx",
        ))?;
      Ok(Some(CloseQueue::InProcess(cmd_tx)))
    }
  }) {
    Ok(queue) => queue,
    Err(VmError::Unimplemented(_)) => None,
    Err(other) => return Err(other),
  };

  let Some(queue) = queue else {
    return Ok(Value::Undefined);
  };

  match queue {
    CloseQueue::InProcess(cmd_tx) => {
      let cmd = WsCommand::Close { code, reason };
      let _ = cmd_tx.try_send(cmd);
    }
    CloseQueue::Ipc(cmd_tx) => {
      let cmd = WebSocketCommand::Close { code, reason };
      let msg = WebSocketIpcCommand::WebSocket { conn_id: ws_id, cmd };
      let _ = cmd_tx.try_send(msg);
    }
  }
  Ok(Value::Undefined)
}

fn decrement_pending_event(env_id: u64, ws_id: u64, payload_bytes: usize) {
  let _ = with_env_state_mut(env_id, |state| {
    if let Some(ws) = state.sockets.get_mut(&ws_id) {
      ws.pending_events = ws.pending_events.saturating_sub(1);
      ws.pending_event_bytes = ws.pending_event_bytes.saturating_sub(payload_bytes);
    }
    Ok(())
  });
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum WsTaskKind {
  /// A regular WebSocket event (`open`, `message`, etc) that is subject to the per-socket pending
  /// task cap.
  Normal,
  /// A close/error delivery task that must be queued even when the per-socket pending task cap has
  /// been reached. Only one close task may be queued per socket.
  Close,
}

fn queue_ws_task<Host: WindowRealmHost + 'static>(
  queue: &ExternalTaskQueueHandle<Host>,
  env_id: u64,
  ws_id: u64,
  kind: WsTaskKind,
  payload_bytes: usize,
  f: impl FnOnce(
      &mut dyn VmHost,
      &mut vm_js::Heap,
      &mut vm_js::Vm,
      &mut VmJsEventLoopHooks<Host>,
      GcObject,
    ) -> Result<(), VmError>
    + Send
    + 'static,
) -> QueueWsTaskOutcome {
  // Enforce per-socket cap first.
  let allowed = with_env_state_mut(env_id, |state| {
    let Some(ws) = state.sockets.get_mut(&ws_id) else {
      return Ok(false);
    };

    // Close tasks are allowed to exceed the normal cap by one, ensuring the JS `close` event is
    // delivered even when we're closing due to event-queue overflow.
    if kind == WsTaskKind::Close {
      debug_assert_eq!(payload_bytes, 0, "close tasks must not account payload bytes");
      if ws.close_task_queued {
        return Ok(false);
      }
      ws.close_task_queued = true;
      ws.pending_events = ws.pending_events.saturating_add(1);
      return Ok(true);
    }

    // If we're already closed, ignore further non-close events.
    if ws.ready_state == WS_CLOSED {
      return Ok(false);
    }

    // If the socket is closing due to a forced close (e.g. event-queue overflow), keep trying to
    // notify the network layer on subsequent events. This is best-effort and helps the IPC backend
    // recover if the renderer->network command channel was temporarily full.
    if ws.ready_state == WS_CLOSING {
      if let Some((code, reason)) = ws.forced_close.clone() {
        if let Some(ipc) = state.ipc.as_ref() {
          let _ = ipc.cmd_tx.try_send(WebSocketIpcCommand::WebSocket {
            conn_id: ws_id,
            cmd: WebSocketCommand::Close {
              code: Some(code),
              reason: Some(reason),
            },
          });
        } else if let Some(cmd_tx) = ws.cmd_tx.as_ref() {
          let _ = cmd_tx.try_send(WsCommand::Close {
            code: Some(code),
            reason: Some(reason),
          });
        }
      }
      return Ok(false);
    }

    let next_bytes = ws.pending_event_bytes.saturating_add(payload_bytes);
    if next_bytes > MAX_WEBSOCKET_PENDING_EVENT_BYTES {
      // Renderer-side backlog has exceeded the byte cap: initiate close rather than silently
      // dropping events (dropping can desync JS state).
      if ws.forced_close.is_none() {
        ws.ready_state = WS_CLOSING;
        let reason = WS_CLOSE_EVENT_QUEUE_OVERFLOW_REASON.to_string();
        ws.forced_close = Some((WS_CLOSE_EVENT_QUEUE_OVERFLOW_CODE, reason.clone()));
        // Best-effort close request:
        // - Legacy in-process backend: the websocket thread also observes `forced_close` and will
        //   close even if the per-socket command queue is full.
        // - IPC backend: notify the network process to close the connection.
        if let Some(ipc) = state.ipc.as_ref() {
          let _ = ipc.cmd_tx.try_send(WebSocketIpcCommand::WebSocket {
            conn_id: ws_id,
            cmd: WebSocketCommand::Close {
              code: Some(WS_CLOSE_EVENT_QUEUE_OVERFLOW_CODE),
              reason: Some(reason),
            },
          });
        } else if let Some(cmd_tx) = ws.cmd_tx.as_ref() {
          let _ = cmd_tx.try_send(WsCommand::Close {
            code: Some(WS_CLOSE_EVENT_QUEUE_OVERFLOW_CODE),
            reason: Some(reason),
          });
        }
      }
      return Ok(false);
    }

    if ws.pending_events >= MAX_QUEUED_WEBSOCKET_EVENTS_PER_SOCKET {
      // Renderer-side backlog has exceeded the cap: initiate close rather than silently dropping
      // events (dropping can desync JS state).
      if ws.forced_close.is_none() {
        ws.ready_state = WS_CLOSING;
        let reason = WS_CLOSE_EVENT_QUEUE_OVERFLOW_REASON.to_string();
        ws.forced_close = Some((WS_CLOSE_EVENT_QUEUE_OVERFLOW_CODE, reason.clone()));
        // Best-effort close request:
        // - Legacy in-process backend: the websocket thread also observes `forced_close` and will
        //   close even if the per-socket command queue is full.
        // - IPC backend: notify the network process to close the connection.
        if let Some(ipc) = state.ipc.as_ref() {
          let _ = ipc.cmd_tx.try_send(WebSocketIpcCommand::WebSocket {
            conn_id: ws_id,
            cmd: WebSocketCommand::Close {
              code: Some(WS_CLOSE_EVENT_QUEUE_OVERFLOW_CODE),
              reason: Some(reason),
            },
          });
        } else if let Some(cmd_tx) = ws.cmd_tx.as_ref() {
          let _ = cmd_tx.try_send(WsCommand::Close {
            code: Some(WS_CLOSE_EVENT_QUEUE_OVERFLOW_CODE),
            reason: Some(reason),
          });
        }
      }
      return Ok(false);
    }

    ws.pending_events = ws.pending_events.saturating_add(1);
    ws.pending_event_bytes = next_bytes;
    Ok(true)
  })
  .unwrap_or(false);
  if !allowed {
    return QueueWsTaskOutcome::Skipped;
  }

  let queue_result = queue.queue_task(TaskSource::Networking, move |host, event_loop| {
    struct PendingGuard {
      env_id: u64,
      ws_id: u64,
      payload_bytes: usize,
    }
    impl Drop for PendingGuard {
      fn drop(&mut self) {
        decrement_pending_event(self.env_id, self.ws_id, self.payload_bytes);
      }
    }
    let _pending = PendingGuard {
      env_id,
      ws_id,
      payload_bytes,
    };

    let mut hooks = VmJsEventLoopHooks::<Host>::new_with_host(host)?;
    hooks.set_event_loop(event_loop);
    let (vm_host, window_realm) = host.vm_host_and_window_realm()?;
    window_realm.reset_interrupt();
    let budget = window_realm.vm_budget_now();
    let (vm, heap) = window_realm.vm_and_heap_mut();
    let mut vm = vm.push_budget(budget);

    let result: crate::error::Result<()> = (|| {
      vm.tick()
        .map_err(|err| vm_error_to_event_loop_error(heap, err))?;

      // If a GC ran since the last time we touched this env, sweep any unreachable WebSocket
      // wrappers and shut down their backing threads.
      let _ = sweep_env_state_if_gc_ran(env_id, heap);

      // Resolve WS object. If the wrapper is no longer reachable, skip dispatch.
      let ws_obj: Option<GcObject> = with_env_state(env_id, |state| {
        Ok(state.sockets.get(&ws_id).and_then(|ws| ws.weak_obj.upgrade(heap)))
      })
      .unwrap_or(None);
      let Some(ws_obj) = ws_obj else {
        // Wrapper is no longer alive: best-effort shutdown and drop Rust-side state.
        let _ = with_env_state_mut(env_id, |state| {
          if let Some(ws) = state.sockets.remove(&ws_id) {
            shutdown_ws_state_locked(state, ws_id, ws);
          }
          Ok(())
        });
        return Ok(());
      };

      let call_result = (|| {
        let result = f(vm_host, heap, &mut vm, &mut hooks, ws_obj);
        match result {
          Ok(()) => Ok(()),
          Err(err) => match err {
            VmError::Throw(_) | VmError::ThrowWithStack { .. } => Ok(()),
            other => Err(other),
          },
        }
      })();

      match call_result {
        Ok(()) => Ok(()),
        Err(err) => Err(vm_error_to_event_loop_error(heap, err)),
      }
    })();

    if let Some(err) = hooks.finish(heap) {
      return Err(err);
    }

    result
  });

  if queue_result.is_err() {
    decrement_pending_event(env_id, ws_id, payload_bytes);
    if kind == WsTaskKind::Close {
      // If we failed to enqueue the close task (e.g. external task queue full/closed), allow a
      // later attempt. This is best-effort and only affects the close task; normal tasks remain
      // capped by `MAX_QUEUED_WEBSOCKET_EVENTS_PER_SOCKET`.
      let _ = with_env_state_mut(env_id, |state| {
        if let Some(ws) = state.sockets.get_mut(&ws_id) {
          ws.close_task_queued = false;
        }
        Ok(())
      });
    }

    // Renderer-side callback delivery failed (external task queue is full/closed). Treat this as
    // backpressure and close the connection to avoid buffering/dropping data indefinitely.
    //
    // This mirrors the per-socket event queue overflow path above, but is triggered by the global
    // event loop queue being unable to accept work.
    let _ = with_env_state_mut(env_id, |state| {
      let Some(ws) = state.sockets.get_mut(&ws_id) else {
        return Ok(());
      };

      // Idempotent: only request forced close once.
      if ws.forced_close.is_none() && ws.ready_state != WS_CLOSED {
        ws.ready_state = WS_CLOSING;
        let reason = WS_CLOSE_EVENT_QUEUE_OVERFLOW_REASON.to_string();
        ws.forced_close = Some((WS_CLOSE_EVENT_QUEUE_OVERFLOW_CODE, reason.clone()));

        // Best-effort close request:
        // - In-process backend: websocket thread observes `forced_close`.
        // - IPC backend: notify the network process.
        if let Some(ipc) = state.ipc.as_ref() {
          let _ = ipc.cmd_tx.try_send(WebSocketIpcCommand::WebSocket {
            conn_id: ws_id,
            cmd: WebSocketCommand::Close {
              code: Some(WS_CLOSE_EVENT_QUEUE_OVERFLOW_CODE),
              reason: Some(reason),
            },
          });
        } else if let Some(cmd_tx) = ws.cmd_tx.as_ref() {
          let _ = cmd_tx.try_send(WsCommand::Close {
            code: Some(WS_CLOSE_EVENT_QUEUE_OVERFLOW_CODE),
            reason: Some(reason),
          });
        }
      }

      Ok(())
    });

    return QueueWsTaskOutcome::DeliveryFailed;
  }

  QueueWsTaskOutcome::Queued
}

fn dispatch_ws_event(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  ws_obj: GcObject,
  event_obj: Value,
  handler_prop: &str,
) -> Result<(), VmError> {
  scope.push_root(Value::Object(ws_obj))?;
  scope.push_root(event_obj)?;

  let dispatch_key = alloc_key(scope, "dispatchEvent")?;
  let dispatch_fn = vm.get_with_host_and_hooks(host, scope, hooks, ws_obj, dispatch_key)?;
  if scope.heap().is_callable(dispatch_fn).unwrap_or(false) {
    let _ = vm.call_with_host_and_hooks(host, scope, hooks, dispatch_fn, Value::Object(ws_obj), &[event_obj]);
  }

  let handler_key = alloc_key(scope, handler_prop)?;
  let handler_val = vm.get_with_host_and_hooks(host, scope, hooks, ws_obj, handler_key)?;
  if scope.heap().is_callable(handler_val).unwrap_or(false) {
    let _ = vm.call_with_host_and_hooks(host, scope, hooks, handler_val, Value::Object(ws_obj), &[event_obj]);
  }
  Ok(())
}

fn make_simple_event(scope: &mut Scope<'_>, event_type: &str) -> Result<GcObject, VmError> {
  let ev = scope.alloc_object()?;
  scope.push_root(Value::Object(ev))?;
  let type_s = scope.alloc_string(event_type)?;
  scope.push_root(Value::String(type_s))?;
  let type_key = alloc_key(scope, "type")?;
  scope.define_property(ev, type_key, data_desc(Value::String(type_s), false))?;
  Ok(ev)
}

fn validate_ws_subprotocol_handshake_response(
  requested_protocols: &[String],
  response_headers: &http::HeaderMap,
) -> Result<String, ()> {
  let mut values = response_headers.get_all("Sec-WebSocket-Protocol").iter();
  let Some(value) = values.next() else {
    // No protocol selected.
    return Ok(String::new());
  };

  // Multiple protocol headers are invalid for WebSocket subprotocol negotiation.
  if values.next().is_some() {
    return Err(());
  }

  let value = value.to_str().map_err(|_| ())?;
  if value.is_empty() {
    return Err(());
  }

  // The server's Sec-WebSocket-Protocol must be a single token (no commas/whitespace).
  if value
    .bytes()
    .any(|b| b == b',' || b.is_ascii_whitespace())
  {
    return Err(());
  }

  if requested_protocols.is_empty() {
    // If no subprotocols were requested, the server must not select one.
    return Err(());
  }

  if !requested_protocols.iter().any(|p| p == value) {
    return Err(());
  }

  Ok(value.to_string())
}

fn poll_connect_abort(cmd_rx: &mpsc::Receiver<WsCommand>) -> Option<WsCommand> {
  loop {
    match cmd_rx.try_recv() {
      Ok(cmd @ WsCommand::Shutdown) => return Some(cmd),
      Ok(cmd @ WsCommand::Close { .. }) => return Some(cmd),
      Ok(_) => continue,
      Err(mpsc::TryRecvError::Empty) => return None,
      Err(mpsc::TryRecvError::Disconnected) => return Some(WsCommand::Shutdown),
    }
  }
}

struct WsConnectFailure {
  close_code: u16,
  close_reason: String,
  dispatch_error: bool,
}

fn ws_fail_timeout(stage: &'static str) -> WsConnectFailure {
  WsConnectFailure {
    close_code: 1006,
    close_reason: format!("{stage} timeout"),
    dispatch_error: true,
  }
}

fn ws_fail_io(stage: &'static str, err: std::io::Error) -> WsConnectFailure {
  let msg = format!("{stage} failed: {}", err);
  WsConnectFailure {
    close_code: 1006,
    close_reason: msg,
    dispatch_error: true,
  }
}

fn ws_fail_ws(stage: &'static str, err: tungstenite::Error) -> WsConnectFailure {
  WsConnectFailure {
    close_code: 1006,
    close_reason: format!("{stage} failed: {err}"),
    dispatch_error: true,
  }
}

fn resolve_socket_addrs_with_deadline(
  host: &str,
  port: u16,
  connect_deadline: Instant,
  cmd_rx: &mpsc::Receiver<WsCommand>,
) -> Result<Vec<SocketAddr>, WsConnectFailure> {
  if let Ok(ip) = host.parse::<IpAddr>() {
    return Ok(vec![SocketAddr::new(ip, port)]);
  }

  let (resp_tx, resp_rx) = mpsc::channel::<Result<Vec<SocketAddr>, std::io::Error>>();
  match dns_lookup_tx().try_send(DnsLookupRequest {
    host: host.to_string(),
    port,
    resp: resp_tx,
  }) {
    Ok(()) => {}
    Err(mpsc::TrySendError::Full(_)) => {
      return Err(WsConnectFailure {
        close_code: 1006,
        close_reason: "DNS resolution queue is full".to_string(),
        dispatch_error: true,
      })
    }
    Err(mpsc::TrySendError::Disconnected(_)) => {
      return Err(WsConnectFailure {
        close_code: 1006,
        close_reason: "DNS resolver is unavailable".to_string(),
        dispatch_error: true,
      })
    }
  }

  // Use a small poll interval so `Shutdown` can abort quickly.
  const DNS_POLL: Duration = Duration::from_millis(50);
  loop {
    if let Some(cmd) = poll_connect_abort(cmd_rx) {
      return Err(match cmd {
        WsCommand::Shutdown => WsConnectFailure {
          close_code: 1001,
          close_reason: "shutdown".to_string(),
          dispatch_error: false,
        },
        WsCommand::Close { code, reason } => WsConnectFailure {
          close_code: code.unwrap_or(1000),
          close_reason: reason.unwrap_or_default(),
          dispatch_error: false,
        },
        _ => WsConnectFailure {
          close_code: 1001,
          close_reason: "shutdown".to_string(),
          dispatch_error: false,
        },
      });
    }
    let now = Instant::now();
    if now >= connect_deadline {
      return Err(ws_fail_timeout("connect"));
    }
    let wait = connect_deadline.saturating_duration_since(now).min(DNS_POLL);
    match resp_rx.recv_timeout(wait) {
      Ok(Ok(addrs)) => return Ok(addrs),
      Ok(Err(err)) => return Err(ws_fail_io("DNS resolution", err)),
      Err(mpsc::RecvTimeoutError::Timeout) => {}
      Err(mpsc::RecvTimeoutError::Disconnected) => {
        return Err(ws_fail_io(
          "DNS resolution",
          std::io::Error::new(std::io::ErrorKind::Other, "DNS worker disconnected"),
        ))
      }
    }
  }
}

fn connect_websocket_with_timeouts(
  request: http::Request<()>,
  url: &url::Url,
  timeouts: WindowWebSocketTimeouts,
  cmd_rx: &mpsc::Receiver<WsCommand>,
) -> Result<
  (
    tungstenite::WebSocket<tungstenite::stream::MaybeTlsStream<std::net::TcpStream>>,
    tungstenite::handshake::client::Response,
  ),
  WsConnectFailure,
> {
  // Cancellation granularity for TCP connect. Keep this reasonably small so `Shutdown` can abort a
  // stalled connect quickly, but large enough to tolerate real-world RTT during the SYN handshake.
  const CONNECT_POLL: Duration = Duration::from_secs(1);
  const HANDSHAKE_POLL: Duration = Duration::from_millis(50);

  if let Some(cmd) = poll_connect_abort(cmd_rx) {
    return Err(match cmd {
      WsCommand::Shutdown => WsConnectFailure {
        close_code: 1001,
        close_reason: "shutdown".to_string(),
        dispatch_error: false,
      },
      WsCommand::Close { code, reason } => WsConnectFailure {
        close_code: code.unwrap_or(1000),
        close_reason: reason.unwrap_or_default(),
        dispatch_error: false,
      },
      _ => WsConnectFailure {
        close_code: 1001,
        close_reason: "shutdown".to_string(),
        dispatch_error: false,
      },
    });
  }

  let host = url
    .host_str()
    .ok_or_else(|| WsConnectFailure {
      close_code: 1006,
      close_reason: "WebSocket URL is missing a host".to_string(),
      dispatch_error: true,
    })?;
  let port = url
    .port_or_known_default()
    .ok_or_else(|| WsConnectFailure {
      close_code: 1006,
      close_reason: "WebSocket URL is missing a port".to_string(),
      dispatch_error: true,
    })?;

  // -------------------------------------------------------------------------
  // DNS + TCP connect
  // -------------------------------------------------------------------------
  let connect_deadline = Instant::now() + timeouts.dns_tcp_connect;
  let addrs = resolve_socket_addrs_with_deadline(host, port, connect_deadline, cmd_rx)?;
  if addrs.is_empty() {
    return Err(WsConnectFailure {
      close_code: 1006,
      close_reason: "DNS resolution returned no addresses".to_string(),
      dispatch_error: true,
    });
  }

  let mut last_connect_err: Option<std::io::Error> = None;
  let mut addr_index: usize = 0;
  // If we observe immediate (non-timeout) errors for every resolved address, return early instead
  // of spinning until the connect deadline.
  let mut consecutive_non_timeout_errors: usize = 0;
  let mut tcp_stream = loop {
    if let Some(cmd) = poll_connect_abort(cmd_rx) {
      return Err(match cmd {
        WsCommand::Shutdown => WsConnectFailure {
          close_code: 1001,
          close_reason: "shutdown".to_string(),
          dispatch_error: false,
        },
        WsCommand::Close { code, reason } => WsConnectFailure {
          close_code: code.unwrap_or(1000),
          close_reason: reason.unwrap_or_default(),
          dispatch_error: false,
        },
        _ => WsConnectFailure {
          close_code: 1001,
          close_reason: "shutdown".to_string(),
          dispatch_error: false,
        },
      });
    }
    let now = Instant::now();
    if now >= connect_deadline {
      if let Some(err) = last_connect_err.take() {
        return Err(ws_fail_io("connect", err));
      }
      return Err(ws_fail_timeout("connect"));
    }
    let remaining = connect_deadline.saturating_duration_since(now);
    let attempt_timeout = remaining.min(CONNECT_POLL);
    let addr = addrs[addr_index];
    addr_index = (addr_index + 1) % addrs.len();
    match std::net::TcpStream::connect_timeout(&addr, attempt_timeout) {
      Ok(stream) => break stream,
      Err(err) => {
        if err.kind() == std::io::ErrorKind::Interrupted {
          continue;
        }
        if err.kind() == std::io::ErrorKind::TimedOut {
          consecutive_non_timeout_errors = 0;
        } else {
          consecutive_non_timeout_errors = consecutive_non_timeout_errors.saturating_add(1);
          if consecutive_non_timeout_errors >= addrs.len() {
            return Err(ws_fail_io("connect", err));
          }
        }
        last_connect_err = Some(err);
      }
    }
  };

  let _ = tcp_stream.set_nodelay(true);

  // -------------------------------------------------------------------------
  // TLS handshake (wss://)
  // -------------------------------------------------------------------------
  let stream: tungstenite::stream::MaybeTlsStream<std::net::TcpStream> = match url.scheme() {
    "ws" => tungstenite::stream::MaybeTlsStream::Plain(tcp_stream),
    "wss" => {
      use std::sync::Arc;

      static TLS_CONFIG: OnceLock<Arc<rustls::ClientConfig>> = OnceLock::new();
      let config = TLS_CONFIG
        .get_or_init(|| {
          let mut roots = rustls::RootCertStore::empty();
          roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
          Arc::new(
            rustls::ClientConfig::builder()
              .with_root_certificates(roots)
              .with_no_client_auth(),
          )
        })
        .clone();

      let server_name = rustls::pki_types::ServerName::try_from(host.to_string()).map_err(|_| WsConnectFailure {
        close_code: 1006,
        close_reason: format!("invalid TLS server name: {host:?}"),
        dispatch_error: true,
      })?;
      let mut conn =
        rustls::ClientConnection::new(config, server_name).map_err(|err| WsConnectFailure {
          close_code: 1006,
          close_reason: format!("TLS setup failed: {err}"),
          dispatch_error: true,
        })?;

      let tls_deadline = Instant::now() + timeouts.tls_handshake;
      let _ = tcp_stream.set_read_timeout(Some(HANDSHAKE_POLL));
      let _ = tcp_stream.set_write_timeout(Some(HANDSHAKE_POLL));
      while conn.is_handshaking() {
        if let Some(cmd) = poll_connect_abort(cmd_rx) {
          return Err(match cmd {
            WsCommand::Shutdown => WsConnectFailure {
              close_code: 1001,
              close_reason: "shutdown".to_string(),
              dispatch_error: false,
            },
            WsCommand::Close { code, reason } => WsConnectFailure {
              close_code: code.unwrap_or(1000),
              close_reason: reason.unwrap_or_default(),
              dispatch_error: false,
            },
            _ => WsConnectFailure {
              close_code: 1001,
              close_reason: "shutdown".to_string(),
              dispatch_error: false,
            },
          });
        }
        if Instant::now() >= tls_deadline {
          return Err(ws_fail_timeout("TLS handshake"));
        }
        match conn.complete_io(&mut tcp_stream) {
          Ok(_) => {}
          Err(ref err)
            if matches!(err.kind(), std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock) => {}
          Err(err) => return Err(ws_fail_io("TLS handshake", err)),
        }
      }

      let tls_stream = rustls::StreamOwned::new(conn, tcp_stream);
      tungstenite::stream::MaybeTlsStream::Rustls(tls_stream)
    }
    other => {
      return Err(WsConnectFailure {
        close_code: 1006,
        close_reason: format!("unsupported WebSocket scheme {other:?}"),
        dispatch_error: true,
      })
    }
  };

  // -------------------------------------------------------------------------
  // WebSocket (HTTP upgrade) handshake
  // -------------------------------------------------------------------------
  match &stream {
    tungstenite::stream::MaybeTlsStream::Plain(s) => {
      let _ = s.set_nonblocking(true);
    }
    tungstenite::stream::MaybeTlsStream::Rustls(s) => {
      let _ = s.get_ref().set_nonblocking(true);
    }
    #[allow(unreachable_patterns)]
    _ => {}
  }

  let ws_deadline = Instant::now() + timeouts.websocket_handshake;

  // Tungstenite's handshake state machine supports non-blocking streams via `HandshakeError::Interrupted`.
  let mut hs = tungstenite::handshake::client::ClientHandshake::start(stream, request, None)
    .map_err(|err| ws_fail_ws("WebSocket handshake", err))?;

  loop {
    if let Some(cmd) = poll_connect_abort(cmd_rx) {
      return Err(match cmd {
        WsCommand::Shutdown => WsConnectFailure {
          close_code: 1001,
          close_reason: "shutdown".to_string(),
          dispatch_error: false,
        },
        WsCommand::Close { code, reason } => WsConnectFailure {
          close_code: code.unwrap_or(1000),
          close_reason: reason.unwrap_or_default(),
          dispatch_error: false,
        },
        _ => WsConnectFailure {
          close_code: 1001,
          close_reason: "shutdown".to_string(),
          dispatch_error: false,
        },
      });
    }
    if Instant::now() >= ws_deadline {
      return Err(ws_fail_timeout("WebSocket handshake"));
    }

    match hs.handshake() {
      Ok((socket, response)) => {
        // Restore blocking mode for the main I/O loop (which uses read timeouts instead of
        // non-blocking sockets for responsiveness).
        match socket.get_ref() {
          tungstenite::stream::MaybeTlsStream::Plain(s) => {
            let _ = s.set_nonblocking(false);
            let _ = s.set_read_timeout(None);
            let _ = s.set_write_timeout(None);
          }
          tungstenite::stream::MaybeTlsStream::Rustls(s) => {
            let tcp = s.get_ref();
            let _ = tcp.set_nonblocking(false);
            let _ = tcp.set_read_timeout(None);
            let _ = tcp.set_write_timeout(None);
          }
          #[allow(unreachable_patterns)]
          _ => {}
        }
        return Ok((socket, response));
      }
      Err(tungstenite::handshake::HandshakeError::Interrupted(mid)) => {
        hs = mid;
        std::thread::sleep(HANDSHAKE_POLL);
      }
      Err(tungstenite::handshake::HandshakeError::Failure(err)) => {
        return Err(ws_fail_ws("WebSocket handshake", err))
      }
    }
  }
}

fn ensure_ipc_event_thread_started<Host: WindowRealmHost + 'static>(
  env_id: u64,
  task_queue: ExternalTaskQueueHandle<Host>,
) -> Result<(), VmError> {
  let init: Option<(mpsc::Receiver<WebSocketIpcEvent>, Arc<AtomicBool>)> =
    with_env_state_mut(env_id, |state| {
      let Some(ipc) = state.ipc.as_mut() else {
        return Ok(None);
      };
      if ipc.thread.is_some() {
        return Ok(None);
      }
      let rx = ipc
        .event_rx
        .take()
        .ok_or(VmError::InvariantViolation(
          "WebSocket IPC env missing event receiver",
        ))?;
      Ok(Some((rx, ipc.stop.clone())))
    })?;

  let Some((rx, stop)) = init else {
    return Ok(());
  };

  // `std::thread::spawn` will panic if the OS refuses to create a thread (e.g. RLIMIT_NPROC or
  // memory pressure). Treat that as a normal VM error instead so untrusted JS cannot crash the
  // renderer.
  //
  // We also avoid moving the IPC receiver into the spawn closure directly so we can restore it to
  // the env state if thread creation fails.
  let (ready_tx, ready_rx) = mpsc::channel::<mpsc::Receiver<WebSocketIpcEvent>>();
  let thread_env_id = env_id;
  let handle = match std::thread::Builder::new()
    .name(format!("ws-ipc-events-{env_id}"))
    .spawn(move || {
      let Ok(rx) = ready_rx.recv() else {
        return;
      };
      websocket_ipc_event_thread_main::<Host>(thread_env_id, rx, task_queue, stop)
    }) {
    Ok(handle) => {
      // Hand off the actual IPC receiver to the spawned thread.
      let _ = ready_tx.send(rx);
      handle
    }
    Err(_err) => {
      // Restore the receiver so a later attempt can retry starting the thread.
      let _ = with_env_state_mut(env_id, |state| {
        if let Some(ipc) = state.ipc.as_mut() {
          if ipc.event_rx.is_none() {
            ipc.event_rx = Some(rx);
          }
        }
        Ok(())
      });
      return Err(VmError::TypeError("failed to start WebSocket IPC event thread"));
    }
  };
  let _ = with_env_state_mut(env_id, |state| {
    if let Some(ipc) = state.ipc.as_mut() {
      ipc.thread = Some(handle);
    }
    Ok(())
  });
  Ok(())
}

fn websocket_ipc_event_thread_main<Host: WindowRealmHost + 'static>(
  env_id: u64,
  rx: mpsc::Receiver<WebSocketIpcEvent>,
  task_queue: ExternalTaskQueueHandle<Host>,
  stop: Arc<AtomicBool>,
) {
  // Keep timeouts small so `WindowWebSocketBindings` teardown can join this thread quickly.
  let poll_timeout = Duration::from_millis(50);
  while !stop.load(Ordering::Relaxed) {
    match rx.recv_timeout(poll_timeout) {
      Ok(ev) => handle_ipc_event::<Host>(&task_queue, env_id, ev),
      Err(mpsc::RecvTimeoutError::Timeout) => {}
      Err(mpsc::RecvTimeoutError::Disconnected) => break,
    }
  }
}

fn handle_ipc_event<Host: WindowRealmHost + 'static>(
  task_queue: &ExternalTaskQueueHandle<Host>,
  env_id: u64,
  ev: WebSocketIpcEvent,
) {
  let (ws_id, event) = match ev {
    WebSocketIpcEvent::WebSocket { conn_id, event } => (conn_id, event),
  };

  match event {
    WebSocketEvent::Open { selected_protocol } => {
      #[derive(Clone, Copy, Debug)]
      enum OpenAction {
        Ignore,
        DispatchOpen,
        DispatchErrorClose,
      }

      let action = with_env_state_mut(env_id, move |state| {
        let Some(ws) = state.sockets.get_mut(&ws_id) else {
          return Ok(OpenAction::Ignore);
        };

        // Open is only valid as a CONNECTING -> OPEN transition.
        if ws.ready_state != WS_CONNECTING {
          return Ok(OpenAction::Ignore);
        }

        let protocol_str = selected_protocol.as_str();
        let valid = if protocol_str.is_empty() {
          true
        } else if ws.requested_protocols.is_empty() {
          false
        } else {
          ws.requested_protocols.iter().any(|p| p == protocol_str)
        };

        if valid {
          ws.ready_state = WS_OPEN;
          ws.protocol = selected_protocol;
          return Ok(OpenAction::DispatchOpen);
        }
        // Treat the network process as untrusted input. If it reports a selected subprotocol that
        // was not requested, fail the connection and close.
        ws.ready_state = WS_CLOSED;
        ws.buffered_amount = 0;
        ws.protocol.clear();

        // Best-effort shutdown request: notify the network process to tear down any underlying
        // connection associated with this `conn_id`.
        if let Some(ipc) = state.ipc.as_ref() {
          let _ = ipc.cmd_tx.try_send(WebSocketIpcCommand::WebSocket {
            conn_id: ws_id,
            cmd: WebSocketCommand::Shutdown,
          });
        }

        Ok(OpenAction::DispatchErrorClose)
      })
      .unwrap_or(OpenAction::Ignore);

      match action {
        OpenAction::Ignore => {}
        OpenAction::DispatchOpen => {
          queue_ws_task::<Host>(
            task_queue,
            env_id,
            ws_id,
            WsTaskKind::Normal,
            0,
            |vm_host, heap, vm, hooks, ws_obj| {
              let mut scope = heap.scope();
              let ev = make_simple_event(&mut scope, "open")?;
              dispatch_ws_event(vm, &mut scope, vm_host, hooks, ws_obj, Value::Object(ev), "onopen")?;
              Ok(())
            },
          );
        }
        OpenAction::DispatchErrorClose => {
          queue_ws_task::<Host>(
            task_queue,
            env_id,
            ws_id,
            WsTaskKind::Close,
            0,
            move |vm_host, heap, vm, hooks, ws_obj| {
              let (url_snapshot, protocol_snapshot) = with_env_state(env_id, |state| {
                Ok(
                  state
                    .sockets
                    .get(&ws_id)
                    .map(|ws| (ws.url.clone(), ws.protocol.clone()))
                    .unwrap_or_default(),
                )
              })
              .unwrap_or_default();
              let mut scope = heap.scope();
              let ev = make_simple_event(&mut scope, "error")?;
              dispatch_ws_event(vm, &mut scope, vm_host, hooks, ws_obj, Value::Object(ev), "onerror")?;
              let close_ev = make_simple_event(&mut scope, "close")?;
              dispatch_ws_event(vm, &mut scope, vm_host, hooks, ws_obj, Value::Object(close_ev), "onclose")?;

              let _ = set_ws_tombstone_props(
                &mut scope,
                ws_obj,
                &url_snapshot,
                &protocol_snapshot,
                WS_CLOSED,
                0,
              );
              let _ = with_env_state_mut(env_id, |state| Ok(state.sockets.remove(&ws_id)));
              Ok(())
            },
          );
        }
      }
    }
    WebSocketEvent::MessageText { text } => {
      let payload_bytes = text.as_bytes().len();
      if payload_bytes > MAX_WEBSOCKET_MESSAGE_BYTES_USIZE {
        return;
      }
      queue_ws_task::<Host>(
        task_queue,
        env_id,
        ws_id,
        WsTaskKind::Normal,
        payload_bytes,
        move |vm_host, heap, vm, hooks, ws_obj| {
          let mut scope = heap.scope();
          let ev = make_simple_event(&mut scope, "message")?;
          let data_s = scope.alloc_string(&text)?;
          scope.push_root(Value::String(data_s))?;
          let data_key = alloc_key(&mut scope, "data")?;
          scope.define_property(ev, data_key, data_desc(Value::String(data_s), false))?;
          dispatch_ws_event(vm, &mut scope, vm_host, hooks, ws_obj, Value::Object(ev), "onmessage")?;
          Ok(())
        },
      );
    }
    WebSocketEvent::MessageBinary { data } => {
      let payload_bytes = data.len();
      if payload_bytes > MAX_WEBSOCKET_MESSAGE_BYTES_USIZE {
        return;
      }
      queue_ws_task::<Host>(
        task_queue,
        env_id,
        ws_id,
        WsTaskKind::Normal,
        payload_bytes,
        move |vm_host, heap, vm, hooks, ws_obj| {
          let intr = vm.intrinsics().ok_or(VmError::Unimplemented(
            "WebSocket message dispatch requires intrinsics",
          ))?;
          let mut scope = heap.scope();
          let ev = make_simple_event(&mut scope, "message")?;

          let (binary_type, realm_id) = with_env_state(env_id, |state| {
            let kind = state
              .sockets
              .get(&ws_id)
              .map(|ws| ws.binary_type)
              .unwrap_or_default();
            Ok((kind, state.realm_id))
          })
          .unwrap_or((WebSocketBinaryType::default(), RealmId::from_raw(0)));

          let data_val: Value = match binary_type {
            WebSocketBinaryType::ArrayBuffer => {
              let ab = scope.alloc_array_buffer_from_u8_vec(data)?;
              scope.push_root(Value::Object(ab))?;
              scope
                .heap_mut()
                .object_set_prototype(ab, Some(intr.array_buffer_prototype()))?;
              Value::Object(ab)
            }
            WebSocketBinaryType::Blob => {
              if window_blob::blob_prototype_for_realm(realm_id).is_none() {
                // Best-effort fallback for environments that install WebSocket without Blob.
                let ab = scope.alloc_array_buffer_from_u8_vec(data)?;
                scope.push_root(Value::Object(ab))?;
                scope
                  .heap_mut()
                  .object_set_prototype(ab, Some(intr.array_buffer_prototype()))?;
                Value::Object(ab)
              } else {
                let blob_obj = window_blob::create_blob_for_realm(
                  &mut scope,
                  realm_id,
                  window_blob::BlobData {
                    bytes: data,
                    r#type: String::new(),
                  },
                )?;
                Value::Object(blob_obj)
              }
            }
          };

          scope.push_root(data_val)?;
          let data_key = alloc_key(&mut scope, "data")?;
          scope.define_property(ev, data_key, data_desc(data_val, false))?;
          dispatch_ws_event(vm, &mut scope, vm_host, hooks, ws_obj, Value::Object(ev), "onmessage")?;
          Ok(())
        },
      );
    }
    WebSocketEvent::SendAck { bytes } => {
      let amount = bytes as usize;
      let _ = with_env_state_mut(env_id, |state| {
        if let Some(ws) = state.sockets.get_mut(&ws_id) {
          ws.buffered_amount = ws.buffered_amount.saturating_sub(amount);
        }
        Ok(())
      });
    }
    WebSocketEvent::Error { message: _ } => {
      let _ = with_env_state_mut(env_id, |state| {
        if let Some(ws) = state.sockets.get_mut(&ws_id) {
          ws.ready_state = WS_CLOSED;
          ws.buffered_amount = 0;
        }
        Ok(())
      });
      queue_ws_task::<Host>(
        task_queue,
        env_id,
        ws_id,
        WsTaskKind::Close,
        0,
        move |vm_host, heap, vm, hooks, ws_obj| {
          let (url_snapshot, protocol_snapshot) = with_env_state(env_id, |state| {
            Ok(
              state
                .sockets
                .get(&ws_id)
                .map(|ws| (ws.url.clone(), ws.protocol.clone()))
                .unwrap_or_default(),
            )
          })
          .unwrap_or_default();

          let mut scope = heap.scope();
        let ev = make_simple_event(&mut scope, "error")?;
        dispatch_ws_event(vm, &mut scope, vm_host, hooks, ws_obj, Value::Object(ev), "onerror")?;
        let close_ev = make_simple_event(&mut scope, "close")?;
        dispatch_ws_event(vm, &mut scope, vm_host, hooks, ws_obj, Value::Object(close_ev), "onclose")?;

        let _ = set_ws_tombstone_props(
          &mut scope,
          ws_obj,
          &url_snapshot,
          &protocol_snapshot,
          WS_CLOSED,
          0,
        );
        let _ = with_env_state_mut(env_id, |state| Ok(state.sockets.remove(&ws_id)));
        Ok(())
      },
      );
    }
    WebSocketEvent::Close { code, reason } => {
      let _ = with_env_state_mut(env_id, |state| {
        if let Some(ws) = state.sockets.get_mut(&ws_id) {
          ws.ready_state = WS_CLOSED;
          ws.buffered_amount = 0;
        }
        Ok(())
      });
      queue_ws_task::<Host>(
        task_queue,
        env_id,
        ws_id,
        WsTaskKind::Close,
        0,
        move |vm_host, heap, vm, hooks, ws_obj| {
        let (url_snapshot, protocol_snapshot) = with_env_state(env_id, |state| {
          Ok(
            state
              .sockets
              .get(&ws_id)
              .map(|ws| (ws.url.clone(), ws.protocol.clone()))
              .unwrap_or_default(),
          )
        })
        .unwrap_or_default();
        let mut scope = heap.scope();
        let ev = make_simple_event(&mut scope, "close")?;
        let code_key = alloc_key(&mut scope, "code")?;
        scope.define_property(ev, code_key, data_desc(Value::Number(code as f64), false))?;
        let reason_key = alloc_key(&mut scope, "reason")?;
        let reason_s = scope.alloc_string(&reason)?;
        scope.push_root(Value::String(reason_s))?;
        scope.define_property(ev, reason_key, data_desc(Value::String(reason_s), false))?;
        dispatch_ws_event(vm, &mut scope, vm_host, hooks, ws_obj, Value::Object(ev), "onclose")?;

        let _ = set_ws_tombstone_props(
          &mut scope,
          ws_obj,
          &url_snapshot,
          &protocol_snapshot,
          WS_CLOSED,
          0,
        );
        let _ = with_env_state_mut(env_id, |state| Ok(state.sockets.remove(&ws_id)));
        Ok(())
      },
      );
    }
  }
}

fn websocket_thread_main<Host: WindowRealmHost + 'static>(
  env_id: u64,
  ws_id: u64,
  fetcher: Arc<dyn ResourceFetcher>,
  url: String,
  document_is_secure: bool,
  requested_protocols: Vec<String>,
  cmd_rx: mpsc::Receiver<WsCommand>,
  task_queue: ExternalTaskQueueHandle<Host>,
) {
  #[cfg(test)]
  {
    ACTIVE_WEBSOCKET_THREADS.fetch_add(1, Ordering::Relaxed);
  }
  struct ThreadGuard;
  impl Drop for ThreadGuard {
    fn drop(&mut self) {
      #[cfg(test)]
      {
        ACTIVE_WEBSOCKET_THREADS.fetch_sub(1, Ordering::Relaxed);
      }
    }
  }
  let _thread_guard = ThreadGuard;

  let timeouts = with_env_state(env_id, |state| Ok(state.env.timeouts)).unwrap_or_default();

  // Treat renderer-supplied URLs as untrusted: re-validate and normalize here.
  let parsed_url = match crate::ipc::websocket::validate_and_normalize_url(&url) {
    Ok(url) => url,
    Err(err) => {
      let close_reason = err.to_string();
      with_env_state_mut(env_id, |state| {
        if let Some(ws) = state.sockets.get_mut(&ws_id) {
          ws.ready_state = WS_CLOSED;
          ws.buffered_amount = 0;
        }
        Ok(())
      })
      .ok();
      queue_ws_task::<Host>(
        &task_queue,
        env_id,
        ws_id,
        WsTaskKind::Close,
        0,
        move |vm_host, heap, vm, hooks, ws_obj| {
          let (url_snapshot, protocol_snapshot) = with_env_state(env_id, |state| {
            Ok(
              state
                .sockets
                .get(&ws_id)
                .map(|ws| (ws.url.clone(), ws.protocol.clone()))
                .unwrap_or_else(|| (url.clone(), String::new())),
            )
          })
          .unwrap_or_else(|_| (url.clone(), String::new()));

          let mut scope = heap.scope();
          let err_ev = make_simple_event(&mut scope, "error")?;
          dispatch_ws_event(vm, &mut scope, vm_host, hooks, ws_obj, Value::Object(err_ev), "onerror")?;

          let close_ev = make_simple_event(&mut scope, "close")?;
          let code_key = alloc_key(&mut scope, "code")?;
          scope.define_property(close_ev, code_key, data_desc(Value::Number(1006.0), false))?;
          let reason_key = alloc_key(&mut scope, "reason")?;
          let reason_s = scope.alloc_string(&close_reason)?;
          scope.push_root(Value::String(reason_s))?;
          scope.define_property(close_ev, reason_key, data_desc(Value::String(reason_s), false))?;
          dispatch_ws_event(vm, &mut scope, vm_host, hooks, ws_obj, Value::Object(close_ev), "onclose")?;

          let _ = set_ws_tombstone_props(
            &mut scope,
            ws_obj,
            &url_snapshot,
            &protocol_snapshot,
            WS_CLOSED,
            0,
          );
          let _ = with_env_state_mut(env_id, |state| Ok(state.sockets.remove(&ws_id)));
          Ok(())
        },
      );
      return;
    }
  };

  // Defense in depth: the renderer is untrusted in multiprocess mode.
  // If the browser/network side marks the document as secure, refuse insecure ws:// targets.
  if document_is_secure && parsed_url.scheme() == "ws" {
    with_env_state_mut(env_id, |state| {
      if let Some(ws) = state.sockets.get_mut(&ws_id) {
        ws.ready_state = WS_CLOSED;
        ws.buffered_amount = 0;
      }
      Ok(())
    })
    .ok();
    queue_ws_task::<Host>(
      &task_queue,
      env_id,
      ws_id,
      WsTaskKind::Close,
      0,
      move |vm_host, heap, vm, hooks, ws_obj| {
        let (url_snapshot, protocol_snapshot) = with_env_state(env_id, |state| {
          Ok(
            state
              .sockets
              .get(&ws_id)
              .map(|ws| (ws.url.clone(), ws.protocol.clone()))
              .unwrap_or_else(|| (url.clone(), String::new())),
          )
        })
        .unwrap_or_else(|_| (url.clone(), String::new()));
        let mut scope = heap.scope();
        let ev = make_simple_event(&mut scope, "error")?;
        dispatch_ws_event(vm, &mut scope, vm_host, hooks, ws_obj, Value::Object(ev), "onerror")?;
        let close_ev = make_simple_event(&mut scope, "close")?;
        dispatch_ws_event(vm, &mut scope, vm_host, hooks, ws_obj, Value::Object(close_ev), "onclose")?;

        let _ = set_ws_tombstone_props(
          &mut scope,
          ws_obj,
          &url_snapshot,
          &protocol_snapshot,
          WS_CLOSED,
          0,
        );
        let _ = with_env_state_mut(env_id, |state| Ok(state.sockets.remove(&ws_id)));
        Ok(())
      },
    );
    return;
  }

  let cookie_url = {
    let mut cookie_url = parsed_url.clone();
    cookie_url.set_fragment(None);
    match parsed_url.scheme() {
      "ws" => {
        cookie_url.set_scheme("http").ok();
        Some(cookie_url)
      }
      "wss" => {
        cookie_url.set_scheme("https").ok();
        Some(cookie_url)
      }
      // Should be normalized by validate_and_normalize_url.
      _ => None,
    }
  };

  let mut request = match parsed_url.clone().into_client_request() {
    Ok(req) => req,
    Err(err) => {
      let close_reason = format!("invalid url: {err}");
      with_env_state_mut(env_id, |state| {
        if let Some(ws) = state.sockets.get_mut(&ws_id) {
          ws.ready_state = WS_CLOSED;
          ws.buffered_amount = 0;
        }
        Ok(())
      })
      .ok();
      queue_ws_task::<Host>(
        &task_queue,
        env_id,
        ws_id,
        WsTaskKind::Close,
        0,
        move |vm_host, heap, vm, hooks, ws_obj| {
          let (url_snapshot, protocol_snapshot) = with_env_state(env_id, |state| {
            Ok(
              state
                .sockets
                .get(&ws_id)
                .map(|ws| (ws.url.clone(), ws.protocol.clone()))
                .unwrap_or_else(|| (url.clone(), String::new())),
            )
          })
          .unwrap_or_else(|_| (url.clone(), String::new()));

          let mut scope = heap.scope();
          let err_ev = make_simple_event(&mut scope, "error")?;
          dispatch_ws_event(vm, &mut scope, vm_host, hooks, ws_obj, Value::Object(err_ev), "onerror")?;
          let close_ev = make_simple_event(&mut scope, "close")?;
          let code_key = alloc_key(&mut scope, "code")?;
          scope.define_property(close_ev, code_key, data_desc(Value::Number(1006.0), false))?;
          let reason_key = alloc_key(&mut scope, "reason")?;
          let reason_s = scope.alloc_string(&close_reason)?;
          scope.push_root(Value::String(reason_s))?;
          scope.define_property(close_ev, reason_key, data_desc(Value::String(reason_s), false))?;
          dispatch_ws_event(vm, &mut scope, vm_host, hooks, ws_obj, Value::Object(close_ev), "onclose")?;

          let _ = set_ws_tombstone_props(
            &mut scope,
            ws_obj,
            &url_snapshot,
            &protocol_snapshot,
            WS_CLOSED,
            0,
          );
          let _ = with_env_state_mut(env_id, |state| Ok(state.sockets.remove(&ws_id)));
          Ok(())
        },
      );
      return;
    }
  };

  // Cookie header integration (best-effort).
  if let Some(cookie_url) = cookie_url.as_ref() {
    if let Some(cookie_header_value) = fetcher.cookie_header_value(cookie_url.as_str()) {
      if !cookie_header_value.is_empty() {
        if let Ok(value) = http::HeaderValue::from_str(&cookie_header_value) {
          request.headers_mut().insert(http::header::COOKIE, value);
        }
      }
    }
  }

  // Requested subprotocols.
  if !requested_protocols.is_empty() {
    let joined = requested_protocols.join(", ");
    if let Ok(value) = http::HeaderValue::from_str(&joined) {
      request
        .headers_mut()
        .insert("Sec-WebSocket-Protocol", value);
    }
  }

  let (mut socket, response) = match connect_websocket_with_timeouts(request, &parsed_url, timeouts, &cmd_rx) {
    Ok(pair) => pair,
    Err(failure) => {
      with_env_state_mut(env_id, |state| {
        if let Some(ws) = state.sockets.get_mut(&ws_id) {
          ws.ready_state = WS_CLOSED;
          ws.buffered_amount = 0;
        }
        Ok(())
      })
      .ok();

      let dispatch_error = failure.dispatch_error;
      let close_code = failure.close_code;
      let close_reason = failure.close_reason;
      queue_ws_task::<Host>(
        &task_queue,
        env_id,
        ws_id,
        WsTaskKind::Close,
        0,
        move |vm_host, heap, vm, hooks, ws_obj| {
          let (url_snapshot, protocol_snapshot) = with_env_state(env_id, |state| {
            Ok(
              state
                .sockets
                .get(&ws_id)
                .map(|ws| (ws.url.clone(), ws.protocol.clone()))
                .unwrap_or_else(|| (url.clone(), String::new())),
            )
          })
          .unwrap_or_else(|_| (url.clone(), String::new()));

          let mut scope = heap.scope();
          if dispatch_error {
            let err_ev = make_simple_event(&mut scope, "error")?;
            dispatch_ws_event(vm, &mut scope, vm_host, hooks, ws_obj, Value::Object(err_ev), "onerror")?;
          }
          let close_ev = make_simple_event(&mut scope, "close")?;
          let code_key = alloc_key(&mut scope, "code")?;
          scope.define_property(close_ev, code_key, data_desc(Value::Number(close_code as f64), false))?;
          let reason_key = alloc_key(&mut scope, "reason")?;
          let reason_s = scope.alloc_string(&close_reason)?;
          scope.push_root(Value::String(reason_s))?;
          scope.define_property(close_ev, reason_key, data_desc(Value::String(reason_s), false))?;
          dispatch_ws_event(vm, &mut scope, vm_host, hooks, ws_obj, Value::Object(close_ev), "onclose")?;

          let _ = set_ws_tombstone_props(
            &mut scope,
            ws_obj,
            &url_snapshot,
            &protocol_snapshot,
            WS_CLOSED,
            0,
          );
          let _ = with_env_state_mut(env_id, |state| Ok(state.sockets.remove(&ws_id)));
          Ok(())
        },
      );
      return;
    }
  };

  // Persist any cookies set by the handshake response.
  if let Some(cookie_url) = cookie_url.as_ref() {
    for value in response.headers().get_all(http::header::SET_COOKIE) {
      if let Ok(raw) = value.to_str() {
        fetcher.store_cookie_from_document(cookie_url.as_str(), raw);
      }
    }
  }

  let selected_protocol =
    match validate_ws_subprotocol_handshake_response(&requested_protocols, response.headers()) {
      Ok(protocol) => protocol,
      Err(()) => {
        // RFC6455: If a server responds with a subprotocol not present in the requested list (or
        // sends a protocol when none were requested), the connection must fail.
        with_env_state_mut(env_id, |state| {
          if let Some(ws) = state.sockets.get_mut(&ws_id) {
            ws.ready_state = WS_CLOSED;
            ws.buffered_amount = 0;
          }
          Ok(())
        })
        .ok();
        queue_ws_task::<Host>(
          &task_queue,
          env_id,
          ws_id,
          WsTaskKind::Close,
          0,
          move |vm_host, heap, vm, hooks, ws_obj| {
            let (url_snapshot, protocol_snapshot) = with_env_state(env_id, |state| {
              Ok(
                state
                  .sockets
                  .get(&ws_id)
                  .map(|ws| (ws.url.clone(), ws.protocol.clone()))
                  .unwrap_or_else(|| (url.clone(), String::new())),
              )
            })
            .unwrap_or_else(|_| (url.clone(), String::new()));

            let mut scope = heap.scope();
            let ev = make_simple_event(&mut scope, "error")?;
            dispatch_ws_event(vm, &mut scope, vm_host, hooks, ws_obj, Value::Object(ev), "onerror")?;
            let close_ev = make_simple_event(&mut scope, "close")?;
            dispatch_ws_event(vm, &mut scope, vm_host, hooks, ws_obj, Value::Object(close_ev), "onclose")?;

            let _ = set_ws_tombstone_props(
              &mut scope,
              ws_obj,
              &url_snapshot,
              &protocol_snapshot,
              WS_CLOSED,
              0,
            );
            let _ = with_env_state_mut(env_id, |state| Ok(state.sockets.remove(&ws_id)));
            Ok(())
          },
        );
        return;
      }
    };

  with_env_state_mut(env_id, |state| {
    if let Some(ws) = state.sockets.get_mut(&ws_id) {
      ws.ready_state = WS_OPEN;
      ws.protocol = selected_protocol.clone();
    }
    Ok(())
  })
  .ok();

  let open_outcome = queue_ws_task::<Host>(
    &task_queue,
    env_id,
    ws_id,
    WsTaskKind::Normal,
    0,
    |vm_host, heap, vm, hooks, ws_obj| {
      let mut scope = heap.scope();
      let ev = make_simple_event(&mut scope, "open")?;
      dispatch_ws_event(vm, &mut scope, vm_host, hooks, ws_obj, Value::Object(ev), "onopen")?;
      Ok(())
    },
  );
  if matches!(open_outcome, QueueWsTaskOutcome::DeliveryFailed) {
    // The renderer is unable to enqueue callback delivery onto the JS event loop (e.g. external
    // task queue is full/closed). Treat this as backpressure and close the connection to avoid
    // buffering/dropping data indefinitely.
    let _ = socket.close(None);
    with_env_state_mut(env_id, |state| {
      if let Some(ws) = state.sockets.get_mut(&ws_id) {
        ws.ready_state = WS_CLOSED;
        ws.buffered_amount = 0;
      }
      Ok(())
    })
    .ok();
    return;
  }

  // If the env (or the socket entry) has already been torn down while we were connecting/handshaking
  // (e.g. renderer teardown raced the connect thread), stop immediately. The `WebSocket` object is
  // no longer observable, and continuing to drain queued commands would only delay teardown.
  let still_registered = with_env_state(env_id, |state| Ok(state.sockets.contains_key(&ws_id)))
    .unwrap_or(false);
  if !still_registered {
    return;
  }

  // Keep the socket responsive to shutdown by using small I/O timeouts.
  //
  // NOTE: This is best-effort. For TLS streams we still attempt to apply the timeout to the
  // underlying TCP socket when accessible.
  match socket.get_ref() {
    tungstenite::stream::MaybeTlsStream::Plain(stream) => {
      let _ = stream.set_read_timeout(Some(Duration::from_millis(50)));
      let _ = stream.set_write_timeout(Some(Duration::from_millis(50)));
    }
    tungstenite::stream::MaybeTlsStream::Rustls(stream) => {
      let _ = stream
        .get_ref()
        .set_read_timeout(Some(Duration::from_millis(50)));
      let _ = stream
        .get_ref()
        .set_write_timeout(Some(Duration::from_millis(50)));
    }
    #[allow(unreachable_patterns)]
    _ => {}
  }

  let mut closing: Option<(u16, String)> = None;

  loop {
    // If the env is unregistered (teardown) or the JS wrapper has been swept, exit quickly so
    // `unregister_window_websocket_env` cannot hang waiting for this thread.
    let still_registered = with_env_state(env_id, |state| Ok(state.sockets.contains_key(&ws_id)))
      .unwrap_or(false);
    if !still_registered {
      break;
    }

    // If the renderer has requested a forced close (e.g. because the renderer-side event queue is
    // overflowing), stop immediately and initiate the close handshake.
    //
    // This is intentionally checked before draining the command channel: if outgoing send commands
    // have saturated the channel, we still need a deterministic close path to keep resource usage
    // bounded.
    if closing.is_none() {
      let forced = with_env_state(env_id, |state| {
        Ok(state.sockets.get(&ws_id).and_then(|ws| ws.forced_close.clone()))
      })
      .unwrap_or(None);
      if let Some((code, reason)) = forced {
        // Treat the renderer as untrusted input: never emit reserved/illegal close codes on the
        // wire.
        let code = if is_valid_websocket_close_code(code) { code } else { 1000 };
        closing = Some((code, reason.clone()));
        let frame = CloseFrame {
          code: tungstenite::protocol::frame::coding::CloseCode::from(code),
          reason: Cow::Owned(reason),
        };
        let _ = socket.close(Some(frame));
        break;
      }
    }

    // Drain commands.
    loop {
      match cmd_rx.try_recv() {
        Ok(WsCommand::SendText(s)) => {
          let len = s.as_bytes().len();
          let write_res = socket.write_message(Message::Text(s));
          with_env_state_mut(env_id, |state| {
            if let Some(ws) = state.sockets.get_mut(&ws_id) {
              ws.buffered_amount = ws.buffered_amount.saturating_sub(len);
            }
            Ok(())
          })
          .ok();
          if write_res.is_err() {
            closing = Some((1006, "".to_string()));
            break;
          }
        }
        Ok(WsCommand::SendBinary(bytes)) => {
          let len = bytes.len();
          let write_res = socket.write_message(Message::Binary(bytes));
          with_env_state_mut(env_id, |state| {
            if let Some(ws) = state.sockets.get_mut(&ws_id) {
              ws.buffered_amount = ws.buffered_amount.saturating_sub(len);
            }
            Ok(())
          })
          .ok();
          if write_res.is_err() {
            closing = Some((1006, "".to_string()));
            break;
          }
        }
        Ok(WsCommand::Close { code, reason }) => {
          // Re-validate in the network/thread layer as well: treat the renderer as untrusted.
          // Never emit reserved/illegal close codes on the wire.
          let code = match code {
            Some(code) if is_valid_websocket_close_code(code) => code,
            Some(_) | None => 1000,
          };
          let reason = reason.unwrap_or_default();
          closing = Some((code, reason.clone()));
          let frame = CloseFrame {
            code: tungstenite::protocol::frame::coding::CloseCode::from(code),
            reason: Cow::Owned(reason),
          };
          let _ = socket.close(Some(frame));
          break;
        }
        Ok(WsCommand::Shutdown) => {
          closing = Some((1001, "shutdown".to_string()));
          let _ = socket.close(None);
          break;
        }
        Err(mpsc::TryRecvError::Empty) => break,
        Err(mpsc::TryRecvError::Disconnected) => {
          closing = Some((1001, "shutdown".to_string()));
          let _ = socket.close(None);
          break;
        }
      }
    }

    if closing.is_some() {
      break;
    }

    match socket.read_message() {
      Ok(Message::Text(text)) => {
        let payload_bytes = text.as_bytes().len();
        if payload_bytes > MAX_WEBSOCKET_MESSAGE_BYTES_USIZE {
          closing = Some((1009, "message too large".to_string()));
          let _ = socket.close(None);
          break;
        }

        let outcome = queue_ws_task::<Host>(
          &task_queue,
          env_id,
          ws_id,
          WsTaskKind::Normal,
          payload_bytes,
          move |vm_host, heap, vm, hooks, ws_obj| {
          let mut scope = heap.scope();
          let ev = make_simple_event(&mut scope, "message")?;
          let data_s = scope.alloc_string(&text)?;
          scope.push_root(Value::String(data_s))?;
          let data_key = alloc_key(&mut scope, "data")?;
          scope.define_property(ev, data_key, data_desc(Value::String(data_s), false))?;
          dispatch_ws_event(vm, &mut scope, vm_host, hooks, ws_obj, Value::Object(ev), "onmessage")?;
          Ok(())
        },
        );
        if matches!(outcome, QueueWsTaskOutcome::DeliveryFailed) {
          closing = Some((1001, "backpressure".to_string()));
          let _ = socket.close(None);
          break;
        }
      }
      Ok(Message::Binary(bytes)) => {
        let payload_bytes = bytes.len();
        if payload_bytes > MAX_WEBSOCKET_MESSAGE_BYTES_USIZE {
          closing = Some((1009, "message too large".to_string()));
          let _ = socket.close(None);
          break;
        }

        let outcome = queue_ws_task::<Host>(
          &task_queue,
          env_id,
          ws_id,
          WsTaskKind::Normal,
          payload_bytes,
          move |vm_host, heap, vm, hooks, ws_obj| {
          let intr = vm.intrinsics().ok_or(VmError::Unimplemented(
            "WebSocket message dispatch requires intrinsics",
          ))?;
          let mut scope = heap.scope();
          let ev = make_simple_event(&mut scope, "message")?;

          let (binary_type, realm_id) = with_env_state(env_id, |state| {
            let kind = state
              .sockets
              .get(&ws_id)
              .map(|ws| ws.binary_type)
              .unwrap_or_default();
            Ok((kind, state.realm_id))
          })
          .unwrap_or((WebSocketBinaryType::default(), RealmId::from_raw(0)));

          let data_val: Value = match binary_type {
            WebSocketBinaryType::ArrayBuffer => {
              let ab = scope.alloc_array_buffer_from_u8_vec(bytes)?;
              scope.push_root(Value::Object(ab))?;
              scope
                .heap_mut()
                .object_set_prototype(ab, Some(intr.array_buffer_prototype()))?;
              Value::Object(ab)
            }
            WebSocketBinaryType::Blob => {
              if window_blob::blob_prototype_for_realm(realm_id).is_none() {
                // Best-effort fallback for environments that install WebSocket without Blob.
                let ab = scope.alloc_array_buffer_from_u8_vec(bytes)?;
                scope.push_root(Value::Object(ab))?;
                scope
                  .heap_mut()
                  .object_set_prototype(ab, Some(intr.array_buffer_prototype()))?;
                Value::Object(ab)
              } else {
                let blob_obj = window_blob::create_blob_for_realm(
                  &mut scope,
                  realm_id,
                  window_blob::BlobData {
                    bytes,
                    r#type: String::new(),
                  },
                )?;
                Value::Object(blob_obj)
              }
            }
          };

          scope.push_root(data_val)?;
          let data_key = alloc_key(&mut scope, "data")?;
          scope.define_property(ev, data_key, data_desc(data_val, false))?;
          dispatch_ws_event(vm, &mut scope, vm_host, hooks, ws_obj, Value::Object(ev), "onmessage")?;
          Ok(())
        },
        );
        if matches!(outcome, QueueWsTaskOutcome::DeliveryFailed) {
          closing = Some((1001, "backpressure".to_string()));
          let _ = socket.close(None);
          break;
        }
      }
      Ok(Message::Close(frame)) => {
        let (code, reason) = frame
          .as_ref()
          .map(|f| (u16::from(f.code), f.reason.to_string()))
          .unwrap_or((1000, "".to_string()));
        closing = Some((code, reason));
        // Reply close.
        let _ = socket.close(frame);
        break;
      }
      Ok(Message::Ping(payload)) => {
        // Per RFC 6455, the endpoint must respond to pings with a pong containing the same payload.
        // Browsers do not surface ping/pong frames to JS, so do not enqueue any JS events.
        if socket.write_message(Message::Pong(payload)).is_err() {
          closing = Some((1006, "".to_string()));
          break;
        }
      }
      Ok(Message::Pong(_)) => {}
      Err(tungstenite::Error::Io(ref io)) if io.kind() == std::io::ErrorKind::TimedOut => {}
      Err(tungstenite::Error::Io(ref io)) if io.kind() == std::io::ErrorKind::WouldBlock => {}
      Err(_) => {
        closing = Some((1006, "".to_string()));
        break;
      }
      _ => {}
    }
  }

  with_env_state_mut(env_id, |state| {
    if let Some(ws) = state.sockets.get_mut(&ws_id) {
      ws.ready_state = WS_CLOSED;
      ws.buffered_amount = 0;
    }
    Ok(())
  })
  .ok();

  let (code, reason) = closing.unwrap_or((1000, "".to_string()));

  queue_ws_task::<Host>(
    &task_queue,
    env_id,
    ws_id,
    WsTaskKind::Close,
    0,
    move |vm_host, heap, vm, hooks, ws_obj| {
      let (url_snapshot, protocol_snapshot) = with_env_state(env_id, |state| {
        Ok(
          state
            .sockets
            .get(&ws_id)
            .map(|ws| (ws.url.clone(), ws.protocol.clone()))
            .unwrap_or_default(),
        )
      })
      .unwrap_or_default();
    let mut scope = heap.scope();
    let ev = make_simple_event(&mut scope, "close")?;
    let code_key = alloc_key(&mut scope, "code")?;
    scope.define_property(ev, code_key, data_desc(Value::Number(code as f64), false))?;
    let reason_key = alloc_key(&mut scope, "reason")?;
    let reason_s = scope.alloc_string(&reason)?;
    scope.push_root(Value::String(reason_s))?;
    scope.define_property(ev, reason_key, data_desc(Value::String(reason_s), false))?;
    dispatch_ws_event(vm, &mut scope, vm_host, hooks, ws_obj, Value::Object(ev), "onclose")?;

    let _ = set_ws_tombstone_props(
      &mut scope,
      ws_obj,
      &url_snapshot,
      &protocol_snapshot,
      WS_CLOSED,
      0,
    );
    let _ = with_env_state_mut(env_id, |state| Ok(state.sockets.remove(&ws_id)));
    Ok(())
  },
  );
}

fn install_window_websocket_bindings_for_env_id<Host: WindowRealmHost + 'static>(
  vm: &mut Vm,
  realm: &Realm,
  heap: &mut Heap,
  env_id: u64,
) -> Result<(), VmError> {
  let intr = realm.intrinsics();
  let func_proto = intr.function_prototype();

  let mut scope = heap.scope();
  let global = realm.global_object();
  scope.push_root(Value::Object(global))?;

  // Look up `EventTarget.prototype` installed by `WindowRealm`.
  let event_target_proto = {
    let event_target_key = alloc_key(&mut scope, "EventTarget")?;
    let ctor = scope
      .heap()
      .object_get_own_data_property_value(global, &event_target_key)?
      .and_then(|v| match v {
        Value::Object(obj) => Some(obj),
        _ => None,
      })
      .ok_or(VmError::Unimplemented(
        "EventTarget is not installed on the global object",
      ))?;

    let proto_key = alloc_key(&mut scope, "prototype")?;
    scope
      .heap()
      .object_get_own_data_property_value(ctor, &proto_key)?
      .and_then(|v| match v {
        Value::Object(obj) => Some(obj),
        _ => None,
      })
      .ok_or(VmError::Unimplemented("EventTarget.prototype is missing"))?
  };

  let call_id: NativeFunctionId = vm.register_native_call(websocket_ctor_call)?;
  let construct_id: NativeConstructId = vm.register_native_construct(websocket_ctor_construct::<Host>)?;
  let name_s = scope.alloc_string("WebSocket")?;
  scope.push_root(Value::String(name_s))?;
  let ctor = scope.alloc_native_function_with_slots(
    call_id,
    Some(construct_id),
    name_s,
    1,
    &[Value::Number(env_id as f64)],
  )?;
  scope.push_root(Value::Object(ctor))?;
  scope.heap_mut().object_set_prototype(ctor, Some(func_proto))?;

  // Prototype created by vm-js.
  let Value::Object(proto) = get_data_prop(&mut scope, ctor, "prototype")? else {
    return Err(VmError::InvariantViolation("WebSocket.prototype missing"));
  };
  scope.push_root(Value::Object(proto))?;
  scope
    .heap_mut()
    .object_set_prototype(proto, Some(event_target_proto))?;

  // Constants (also available on instances via prototype).
  for (name, value) in [
    ("CONNECTING", WS_CONNECTING),
    ("OPEN", WS_OPEN),
    ("CLOSING", WS_CLOSING),
    ("CLOSED", WS_CLOSED),
  ] {
    set_data_prop(&mut scope, ctor, name, Value::Number(value as f64), false)?;
    set_data_prop(&mut scope, proto, name, Value::Number(value as f64), false)?;
  }

  // Methods.
  let send_id = vm.register_native_call(websocket_send::<Host>)?;
  let send_name = scope.alloc_string("send")?;
  scope.push_root(Value::String(send_name))?;
  let send_fn = scope.alloc_native_function(send_id, None, send_name, 1)?;
  scope.heap_mut().object_set_prototype(send_fn, Some(func_proto))?;
  set_data_prop(&mut scope, proto, "send", Value::Object(send_fn), true)?;

  let close_id = vm.register_native_call(websocket_close::<Host>)?;
  let close_name = scope.alloc_string("close")?;
  scope.push_root(Value::String(close_name))?;
  let close_fn = scope.alloc_native_function(close_id, None, close_name, 2)?;
  scope.heap_mut().object_set_prototype(close_fn, Some(func_proto))?;
  set_data_prop(&mut scope, proto, "close", Value::Object(close_fn), true)?;

  // Accessors.
  let ready_get_id = vm.register_native_call(websocket_ready_state_get)?;
  let ready_get_name = scope.alloc_string("get readyState")?;
  scope.push_root(Value::String(ready_get_name))?;
  let ready_get_fn = scope.alloc_native_function(ready_get_id, None, ready_get_name, 0)?;
  scope
    .heap_mut()
    .object_set_prototype(ready_get_fn, Some(func_proto))?;
  let ready_key = alloc_key(&mut scope, "readyState")?;
  scope.define_property(
    proto,
    ready_key,
    accessor_desc(Value::Object(ready_get_fn), Value::Undefined),
  )?;

  let url_get_id = vm.register_native_call(websocket_url_get)?;
  let url_get_name = scope.alloc_string("get url")?;
  scope.push_root(Value::String(url_get_name))?;
  let url_get_fn = scope.alloc_native_function(url_get_id, None, url_get_name, 0)?;
  scope
    .heap_mut()
    .object_set_prototype(url_get_fn, Some(func_proto))?;
  let url_key = alloc_key(&mut scope, "url")?;
  scope.define_property(
    proto,
    url_key,
    accessor_desc(Value::Object(url_get_fn), Value::Undefined),
  )?;

  let protocol_get_id = vm.register_native_call(websocket_protocol_get)?;
  let protocol_get_name = scope.alloc_string("get protocol")?;
  scope.push_root(Value::String(protocol_get_name))?;
  let protocol_get_fn = scope.alloc_native_function(protocol_get_id, None, protocol_get_name, 0)?;
  scope
    .heap_mut()
    .object_set_prototype(protocol_get_fn, Some(func_proto))?;
  let protocol_key = alloc_key(&mut scope, "protocol")?;
  scope.define_property(
    proto,
    protocol_key,
    accessor_desc(Value::Object(protocol_get_fn), Value::Undefined),
  )?;

  let buffered_get_id = vm.register_native_call(websocket_buffered_amount_get)?;
  let buffered_get_name = scope.alloc_string("get bufferedAmount")?;
  scope.push_root(Value::String(buffered_get_name))?;
  let buffered_get_fn = scope.alloc_native_function(buffered_get_id, None, buffered_get_name, 0)?;
  scope
    .heap_mut()
    .object_set_prototype(buffered_get_fn, Some(func_proto))?;
  let buffered_key = alloc_key(&mut scope, "bufferedAmount")?;
  scope.define_property(
    proto,
    buffered_key,
    accessor_desc(Value::Object(buffered_get_fn), Value::Undefined),
  )?;

  let binary_get_id = vm.register_native_call(websocket_binary_type_get)?;
  let binary_get_name = scope.alloc_string("get binaryType")?;
  scope.push_root(Value::String(binary_get_name))?;
  let binary_get_fn = scope.alloc_native_function(binary_get_id, None, binary_get_name, 0)?;
  scope
    .heap_mut()
    .object_set_prototype(binary_get_fn, Some(func_proto))?;
  let binary_set_id = vm.register_native_call(websocket_binary_type_set)?;
  let binary_set_name = scope.alloc_string("set binaryType")?;
  scope.push_root(Value::String(binary_set_name))?;
  let binary_set_fn = scope.alloc_native_function(binary_set_id, None, binary_set_name, 1)?;
  scope
    .heap_mut()
    .object_set_prototype(binary_set_fn, Some(func_proto))?;
  let binary_key = alloc_key(&mut scope, "binaryType")?;
  scope.define_property(
    proto,
    binary_key,
    accessor_desc(Value::Object(binary_get_fn), Value::Object(binary_set_fn)),
  )?;

  // Expose on global.
  let ctor_key = alloc_key(&mut scope, "WebSocket")?;
  scope.define_property(global, ctor_key, data_desc(Value::Object(ctor), true))?;

  Ok(())
}

/// Install WebSocket bindings onto the window global object.
///
/// Returns an env id that can be passed to [`unregister_window_websocket_env`] to tear down the
/// backing Rust state when the realm/host is dropped.
pub fn install_window_websocket_bindings<Host: WindowRealmHost + 'static>(
  vm: &mut Vm,
  realm: &Realm,
  heap: &mut Heap,
  env: WindowWebSocketEnv,
) -> Result<u64, VmError> {
  let bindings = install_window_websocket_bindings_with_guard::<Host>(vm, realm, heap, env)?;
  Ok(bindings.disarm())
}

/// Install WebSocket bindings onto the window global object, returning an RAII guard that
/// automatically unregisters the backing Rust state when dropped.
pub fn install_window_websocket_bindings_with_guard<Host: WindowRealmHost + 'static>(
  vm: &mut Vm,
  realm: &Realm,
  heap: &mut Heap,
  env: WindowWebSocketEnv,
) -> Result<WindowWebSocketBindings, VmError> {
  let env_id = NEXT_ENV_ID.fetch_add(1, Ordering::Relaxed);
  {
    let mut lock = envs().lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    lock.insert(env_id, EnvState::new(env, realm.id()));
  }
  let bindings = WindowWebSocketBindings::new(env_id);
  if let Err(err) = install_window_websocket_bindings_for_env_id::<Host>(vm, realm, heap, env_id) {
    unregister_window_websocket_env(env_id);
    return Err(err);
  }

  Ok(bindings)
}

/// Install the IPC-backed WebSocket bindings onto the window global object.
///
/// Returns an env id that can be passed to [`unregister_window_websocket_env`] to tear down the
/// backing Rust state when the realm/host is dropped.
pub fn install_window_websocket_ipc_bindings<Host: WindowRealmHost + 'static>(
  vm: &mut Vm,
  realm: &Realm,
  heap: &mut Heap,
  env: WindowWebSocketIpcEnv,
) -> Result<u64, VmError> {
  let bindings = install_window_websocket_ipc_bindings_with_guard::<Host>(vm, realm, heap, env)?;
  Ok(bindings.disarm())
}

/// Install the IPC-backed WebSocket bindings onto the window global object, returning an RAII guard
/// that automatically unregisters the backing Rust state when dropped.
pub fn install_window_websocket_ipc_bindings_with_guard<Host: WindowRealmHost + 'static>(
  vm: &mut Vm,
  realm: &Realm,
  heap: &mut Heap,
  env: WindowWebSocketIpcEnv,
) -> Result<WindowWebSocketBindings, VmError> {
  let WindowWebSocketIpcEnv {
    fetcher: _,
    document_url,
    cmd_tx,
    event_rx,
  } = env;
  let fetcher: Arc<dyn ResourceFetcher> = Arc::new(IpcNoopFetcher);
  let env_id = NEXT_ENV_ID.fetch_add(1, Ordering::Relaxed);
  let ipc_state = IpcEnvState {
    cmd_tx,
    event_rx: Some(event_rx),
    stop: Arc::new(AtomicBool::new(false)),
    thread: None,
  };
  {
    let mut lock = envs().lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    lock.insert(
      env_id,
      EnvState::new_ipc(WindowWebSocketEnv::for_document(fetcher, document_url), realm.id(), ipc_state),
    );
  }

  let bindings = WindowWebSocketBindings::new(env_id);
  if let Err(err) = install_window_websocket_bindings_for_env_id::<Host>(vm, realm, heap, env_id) {
    unregister_window_websocket_env(env_id);
    return Err(err);
  }

  Ok(bindings)
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::dom2;
  use crate::error::Result;
  use crate::js::{EventLoop, JsExecutionOptions, RunLimits, WindowHost, WindowHostState};
  use crate::resource::{FetchedResource, HttpFetcher};
  use crate::testing::{net_test_lock, try_bind_localhost};
  use selectors::context::QuirksMode;
  use std::io::Read;
  use std::net::TcpListener;
  use std::sync::Arc;
  use std::time::Instant;

  fn get_global_prop_utf8(host: &mut WindowHost, name: &str) -> Option<String> {
    let window = host.host_mut().window_mut();
    let (_vm, realm, heap) = window.vm_realm_and_heap_mut();
    let mut scope = heap.scope();
    let global = realm.global_object();
    scope.push_root(Value::Object(global)).ok()?;
    let key_s = scope.alloc_string(name).ok()?;
    scope.push_root(Value::String(key_s)).ok()?;
    let key = PropertyKey::from_string(key_s);
    let val = scope
      .heap()
      .object_get_own_data_property_value(global, &key)
      .ok()
      .flatten()
      .unwrap_or(Value::Undefined);
    match val {
      Value::String(s) => scope.heap().get_string(s).ok().map(|s| s.to_utf8_lossy()),
      Value::Bool(b) => Some(b.to_string()),
      Value::Number(n) => Some(n.to_string()),
      Value::Undefined => None,
      _ => Some(format!("{val:?}")),
    }
  }

  #[derive(Debug, Default)]
  struct NoFetchResourceFetcher;

  impl ResourceFetcher for NoFetchResourceFetcher {
    fn fetch(&self, url: &str) -> Result<FetchedResource> {
      Err(crate::Error::Other(format!(
        "NoFetchResourceFetcher does not support fetch: {url}"
      )))
    }
  }

  fn make_host(dom: dom2::Document, document_url: impl Into<String>) -> Result<WindowHost> {
    WindowHost::new_with_fetcher(dom, document_url, Arc::new(NoFetchResourceFetcher))
  }

  #[test]
  fn websocket_ctor_rejects_empty_protocol_string() -> Result<()> {
    let dom = dom2::Document::new(QuirksMode::NoQuirks);
    let mut host = make_host(dom, "https://example.invalid/")?;
    let err = host
      // Use wss:// so mixed content checks do not mask protocol validation.
      .exec_script(r#"new WebSocket("wss://example.invalid/", "");"#)
      .expect_err("expected invalid protocols argument to throw");
    assert!(
      err.to_string().contains("WebSocket protocol must not be empty"),
      "unexpected error: {err}"
    );
    Ok(())
  }

  #[test]
  fn websocket_connect_send_echo_close() -> Result<()> {
    let _lock = net_test_lock();
    let Some(listener) = try_bind_localhost("websocket_connect_send_echo_close") else {
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
            // Make the test deterministic: if the client never completes the handshake or never
            // sends a message, we want a bounded failure instead of hanging forever.
            let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
            let _ = stream.set_write_timeout(Some(Duration::from_secs(5)));
            let mut ws = tungstenite::accept(stream).expect("accept websocket");
            let read_deadline = Instant::now() + Duration::from_secs(5);
            let msg = loop {
              match ws.read_message() {
                Ok(msg) => break msg,
                Err(tungstenite::Error::Io(ref err))
                  if matches!(err.kind(), std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock) =>
                {
                  if Instant::now() >= read_deadline {
                    panic!("server read timed out");
                  }
                }
                Err(err) => panic!("server read failed: {err}"),
              }
            };
            ws.write_message(msg).expect("echo");
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

    let dom = dom2::Document::new(QuirksMode::NoQuirks);
    let mut host = make_host(dom, "http://example.invalid/")?;

    host.exec_script(&format!(
      r#"
      globalThis.__done = false;
      globalThis.__err = "";
      globalThis.__msg = "";
      globalThis.__ws = new WebSocket("ws://{addr}/");
      const ws = globalThis.__ws;
      ws.onopen = function () {{
        ws.send("hello");
      }};
      ws.onmessage = function (e) {{
        globalThis.__msg = String(e && e.data);
        ws.close();
      }};
      ws.onerror = function (e) {{
        globalThis.__err = "error";
        globalThis.__done = true;
      }};
      ws.onclose = function () {{
        globalThis.__done = true;
      }};
      "#,
    ))?;

    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
      let _ = host.run_until_idle(RunLimits {
        max_tasks: 100,
        max_microtasks: 1000,
        max_wall_time: Some(Duration::from_millis(50)),
      })?;

      let done = get_global_prop_utf8(&mut host, "__done").unwrap_or_default();
      if done == "true" {
        break;
      }
      if Instant::now() >= deadline {
        break;
      }
      std::thread::sleep(Duration::from_millis(10));
    }

    assert_eq!(
      get_global_prop_utf8(&mut host, "__err").unwrap_or_default(),
      "",
      "unexpected websocket error: {:?}",
      get_global_prop_utf8(&mut host, "__err")
    );
    assert_eq!(
      get_global_prop_utf8(&mut host, "__msg").as_deref(),
      Some("hello")
    );

    server.join().expect("server thread panicked");
    Ok(())
  }

  #[test]
  fn websocket_binary_type_controls_message_data_type() -> Result<()> {
    let _lock = net_test_lock();
    let Some(listener) = try_bind_localhost("websocket_binary_type_controls_message_data_type") else {
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
            let mut ws = tungstenite::accept(stream).expect("accept websocket");

            let read_one = |ws: &mut tungstenite::WebSocket<std::net::TcpStream>| {
              let read_deadline = Instant::now() + Duration::from_secs(5);
              loop {
                match ws.read_message() {
                  Ok(msg) => break msg,
                  Err(tungstenite::Error::Io(ref err))
                    if matches!(
                      err.kind(),
                      std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
                    ) =>
                  {
                    if Instant::now() >= read_deadline {
                      panic!("server read timed out");
                    }
                  }
                  Err(err) => panic!("server read failed: {err}"),
                }
              }
            };

            let msg1 = read_one(&mut ws);
            ws.write_message(msg1).expect("echo 1");
            let msg2 = read_one(&mut ws);
            ws.write_message(msg2).expect("echo 2");
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

    let dom = dom2::Document::new(QuirksMode::NoQuirks);
    let mut host = make_host(dom, "https://example.invalid/")?;

    host.exec_script(&format!(
      r#"
      globalThis.__done = false;
      globalThis.__err = "";

      globalThis.__defaultBinaryType = "";
      globalThis.__invalidBinaryTypeErr = "";
      globalThis.__binaryTypeAfterInvalid = "";
      globalThis.__binaryTypeAfterSet = "";

      globalThis.__firstIsBlob = false;
      globalThis.__firstSize = -1;
      globalThis.__secondIsArrayBuffer = false;
      globalThis.__secondByteLength = -1;

      globalThis.__ws = new WebSocket("ws://{addr}/");
      const ws = globalThis.__ws;

      globalThis.__defaultBinaryType = ws.binaryType;
      try {{
        ws.binaryType = "nope";
      }} catch (e) {{
        globalThis.__invalidBinaryTypeErr = (e && e.name) ? e.name : String(e);
      }}
      globalThis.__binaryTypeAfterInvalid = ws.binaryType;

      let count = 0;
      ws.onopen = function () {{
        ws.send(new Uint8Array([1, 2]));
      }};
      ws.onmessage = function (e) {{
        count++;
        if (count === 1) {{
          globalThis.__firstIsBlob = (typeof Blob === "function") && (e.data instanceof Blob);
          globalThis.__firstSize = (e && e.data && typeof e.data.size === "number") ? e.data.size : -1;
          ws.binaryType = "arraybuffer";
          globalThis.__binaryTypeAfterSet = ws.binaryType;
          ws.send(new Uint8Array([3, 4]));
        }} else if (count === 2) {{
          globalThis.__secondIsArrayBuffer = e && (e.data instanceof ArrayBuffer);
          globalThis.__secondByteLength = (e && e.data && typeof e.data.byteLength === "number")
            ? e.data.byteLength
            : -1;
          ws.close();
        }}
      }};
      ws.onerror = function () {{
        globalThis.__err = "error";
        globalThis.__done = true;
      }};
      ws.onclose = function () {{
        globalThis.__done = true;
      }};
      "#,
    ))?;

    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
      let _ = host.run_until_idle(RunLimits {
        max_tasks: 100,
        max_microtasks: 1000,
        max_wall_time: Some(Duration::from_millis(50)),
      })?;

      let done = get_global_prop_utf8(&mut host, "__done").unwrap_or_default();
      if done == "true" {
        break;
      }
      if Instant::now() >= deadline {
        break;
      }
      std::thread::sleep(Duration::from_millis(10));
    }

    assert_eq!(
      get_global_prop_utf8(&mut host, "__err").unwrap_or_default(),
      "",
      "unexpected websocket error: {:?}",
      get_global_prop_utf8(&mut host, "__err")
    );

    assert_eq!(
      get_global_prop_utf8(&mut host, "__defaultBinaryType").as_deref(),
      Some("blob")
    );
    assert_eq!(
      get_global_prop_utf8(&mut host, "__invalidBinaryTypeErr").as_deref(),
      Some("SyntaxError")
    );
    assert_eq!(
      get_global_prop_utf8(&mut host, "__binaryTypeAfterInvalid").as_deref(),
      Some("blob")
    );

    assert_eq!(
      get_global_prop_utf8(&mut host, "__firstIsBlob").as_deref(),
      Some("true")
    );
    assert_eq!(get_global_prop_utf8(&mut host, "__firstSize").as_deref(), Some("2"));
    assert_eq!(
      get_global_prop_utf8(&mut host, "__binaryTypeAfterSet").as_deref(),
      Some("arraybuffer")
    );
    assert_eq!(
      get_global_prop_utf8(&mut host, "__secondIsArrayBuffer").as_deref(),
      Some("true")
    );
    assert_eq!(
      get_global_prop_utf8(&mut host, "__secondByteLength").as_deref(),
      Some("2")
    );

    server.join().expect("server thread panicked");
    Ok(())
  }

  #[test]
  fn websocket_responds_to_ping_frames() -> Result<()> {
    let _lock = net_test_lock();
    let Some(listener) = try_bind_localhost("websocket_responds_to_ping_frames") else {
      return Ok(());
    };
    listener.set_nonblocking(true).expect("set_nonblocking");
    let addr = listener.local_addr().expect("local_addr");

    let ping_payload: Vec<u8> = b"fastrender-ping".to_vec();
    let ping_payload_server = ping_payload.clone();

    let server = std::thread::spawn(move || {
      let deadline = Instant::now() + Duration::from_secs(5);
      loop {
        match listener.accept() {
          Ok((stream, _)) => {
            let mut stream = stream;
            let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
            let _ = stream.set_write_timeout(Some(Duration::from_secs(5)));
            let mut ws = tungstenite::accept(stream).expect("accept websocket");

            // Send ping and wait for matching pong.
            ws.write_message(Message::Ping(ping_payload_server.clone()))
              .expect("server ping write failed");

            let read_deadline = Instant::now() + Duration::from_secs(5);
            loop {
              match ws.read_message() {
                Ok(Message::Pong(payload)) => {
                  assert_eq!(payload, ping_payload_server, "pong payload mismatch");
                  break;
                }
                Ok(other) => panic!("expected pong, got {other:?}"),
                Err(tungstenite::Error::Io(ref err))
                  if matches!(err.kind(), std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock) =>
                {
                  if Instant::now() >= read_deadline {
                    panic!("server pong read timed out");
                  }
                }
                Err(err) => panic!("server pong read failed: {err}"),
              }
            }

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

    let dom = dom2::Document::new(QuirksMode::NoQuirks);
    let mut host = make_host(dom, "http://example.invalid/")?;

    host.exec_script(&format!(
      r#"
      globalThis.__done = false;
      globalThis.__err = "";
      globalThis.__messageCount = 0;
      globalThis.__ws = new WebSocket("ws://{addr}/");
      const ws = globalThis.__ws;
      ws.onopen = function () {{
        // No-op: server sends ping immediately after handshake.
      }};
      ws.onmessage = function () {{
        globalThis.__messageCount++;
      }};
      ws.onerror = function () {{
        globalThis.__err = "error";
        globalThis.__done = true;
      }};
      ws.onclose = function () {{
        globalThis.__done = true;
      }};
      "#,
    ))?;

    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
      let _ = host.run_until_idle(RunLimits {
        max_tasks: 100,
        max_microtasks: 1000,
        max_wall_time: Some(Duration::from_millis(50)),
      })?;

      let done = get_global_prop_utf8(&mut host, "__done").unwrap_or_default();
      if done == "true" {
        break;
      }
      if Instant::now() >= deadline {
        break;
      }
      std::thread::sleep(Duration::from_millis(10));
    }

    assert_eq!(
      get_global_prop_utf8(&mut host, "__err").unwrap_or_default(),
      "",
      "unexpected websocket error: {:?}",
      get_global_prop_utf8(&mut host, "__err")
    );
    assert_eq!(
      get_global_prop_utf8(&mut host, "__messageCount").as_deref(),
      Some("0"),
      "ping/pong frames must not surface as JS message events"
    );

    server.join().expect("server thread panicked");
    Ok(())
  }

  #[test]
  fn websocket_handshake_timeout_emits_error_and_cleans_up_threads() -> Result<()> {
    let _lock = net_test_lock();
    let Some(listener) = try_bind_localhost("websocket_handshake_timeout_emits_error_and_cleans_up_threads") else {
      return Ok(());
    };
    listener.set_nonblocking(true).expect("set_nonblocking");
    let addr = listener.local_addr().expect("local_addr");

    // Accept the TCP connection but never send a WebSocket handshake response.
    let server = std::thread::spawn(move || {
      let deadline = Instant::now() + Duration::from_secs(5);
      let (mut stream, _) = loop {
        match listener.accept() {
          Ok(pair) => break pair,
          Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
            if Instant::now() >= deadline {
              panic!("accept timed out");
            }
            std::thread::sleep(Duration::from_millis(10));
          }
          Err(e) => panic!("accept failed: {e}"),
        }
      };

      // Best-effort: read the request bytes so the client isn't backpressured.
      let _ = stream.set_read_timeout(Some(Duration::from_millis(100)));
      let mut buf = [0u8; 1024];
      let _ = stream.read(&mut buf);

      // Keep the socket open long enough for the client to hit its handshake timeout.
      std::thread::sleep(Duration::from_secs(1));
    });

    let dom = dom2::Document::new(QuirksMode::NoQuirks);
    let mut host = make_host(dom, "http://example.invalid/")?;

    // Tighten timeouts so the test is fast + deterministic.
    let env_id = host.host().websocket_env_id();
    with_env_state_mut(env_id, |state| {
      state.env.timeouts = WindowWebSocketTimeouts {
        dns_tcp_connect: Duration::from_millis(200),
        tls_handshake: Duration::from_millis(200),
        websocket_handshake: Duration::from_millis(200),
      };
      Ok(())
    })
    .expect("mutate websocket timeouts");

    let before_threads = active_websocket_threads();

    host.exec_script(&format!(
      r#"
      globalThis.__done = false;
      globalThis.__err = "";
      globalThis.__close_code = 0;
      globalThis.__close_reason = "";
      globalThis.__ws = new WebSocket("ws://{addr}/");
      const ws = globalThis.__ws;
      ws.onopen = function () {{
        globalThis.__err = "opened";
        globalThis.__done = true;
      }};
      ws.onerror = function () {{
        globalThis.__err = "error";
      }};
      ws.onclose = function (e) {{
        globalThis.__close_code = Number(e && e.code) || 0;
        globalThis.__close_reason = String(e && e.reason);
        globalThis.__done = true;
      }};
      "#,
    ))?;

    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
      let _ = host.run_until_idle(RunLimits {
        max_tasks: 100,
        max_microtasks: 1000,
        max_wall_time: Some(Duration::from_millis(50)),
      })?;

      let done = get_global_prop_utf8(&mut host, "__done").unwrap_or_default();
      if done == "true" {
        break;
      }
      if Instant::now() >= deadline {
        break;
      }
      std::thread::sleep(Duration::from_millis(10));
    }

    assert_eq!(
      get_global_prop_utf8(&mut host, "__err").unwrap_or_default(),
      "error",
      "expected websocket error event before close"
    );
    assert_eq!(
      get_global_prop_utf8(&mut host, "__close_code").unwrap_or_default(),
      "1006",
      "expected abnormal closure on handshake timeout"
    );
    let reason = get_global_prop_utf8(&mut host, "__close_reason").unwrap_or_default();
    assert!(
      reason.contains("timeout"),
      "expected close reason to mention timeout, got {reason:?}"
    );

    drop(host);

    // Ensure the underlying websocket thread fully terminates.
    let thread_deadline = Instant::now() + Duration::from_secs(2);
    while active_websocket_threads() != before_threads && Instant::now() < thread_deadline {
      std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(active_websocket_threads(), before_threads, "websocket thread leaked");

    server.join().expect("server thread panicked");
    Ok(())
  }

  #[test]
  fn websocket_external_task_queue_overflow_closes_connection() -> Result<()> {
    let _lock = net_test_lock();
    let Some(listener) =
      try_bind_localhost("websocket_external_task_queue_overflow_closes_connection")
    else {
      return Ok(());
    };
    listener.set_nonblocking(true).expect("set_nonblocking");
    let addr = listener.local_addr().expect("local_addr");

    let (server_tx, server_rx) = mpsc::channel::<()>();
    let server = std::thread::spawn(move || {
      let deadline = Instant::now() + Duration::from_secs(5);
      loop {
        match listener.accept() {
          Ok((stream, _)) => {
            let mut stream = stream;
            let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
            let _ = stream.set_write_timeout(Some(Duration::from_secs(5)));
            let mut ws = tungstenite::accept(stream).expect("accept websocket");

            // Keep the server responsive so the test can deterministically assert that the client
            // closes promptly.
            let _ = ws.get_mut().set_read_timeout(Some(Duration::from_millis(50)));

            ws
              .write_message(Message::Text("first".to_string()))
              .expect("server write first");

            // Wait for the client to close promptly after being unable to enqueue the message event.
            let close_deadline = Instant::now() + Duration::from_secs(5);
            loop {
              match ws.read_message() {
                Ok(Message::Close(_)) => break,
                Ok(_) => {}
                Err(tungstenite::Error::Io(ref err))
                  if matches!(
                    err.kind(),
                    std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
                  ) =>
                {
                  if Instant::now() >= close_deadline {
                    panic!("server timed out waiting for client close");
                  }
                }
                Err(_err) => break,
              }
            }

            let _ = server_tx.send(());
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

    let mut options = JsExecutionOptions::default();
    // Keep the external task queue extremely small so we can deterministically force enqueue
    // failures.
    options.event_loop_queue_limits.max_pending_tasks = 2;

    let dom = dom2::Document::new(QuirksMode::NoQuirks);
    let mut host = WindowHost::new_with_fetcher_and_options(
      dom,
      "https://example.invalid/",
      Arc::new(NoFetchResourceFetcher),
      options,
    )?;
    host.event_loop_mut().clear_all_pending_work();

    // Fill the external task queue so the upcoming `open` event can enqueue, but the subsequent
    // message cannot.
    let ext = host.event_loop().external_task_queue_handle();
    ext.queue_task(TaskSource::Networking, |_host, _event_loop| Ok(()))?;

    host.exec_script(&format!(
      r#"
      globalThis.__msg_count = 0;
      globalThis.__ws = new WebSocket("ws://{addr}/");
      globalThis.__ws.onmessage = function (_e) {{
        globalThis.__msg_count++;
      }};
      "#,
    ))?;

    // Ensure the client closes promptly (otherwise this test could hang).
    server_rx
      .recv_timeout(Duration::from_secs(5))
      .expect("server did not observe close");

    // Drain the event loop: if a message event was successfully queued, it would run now.
    let _ = host.run_until_idle(RunLimits {
      max_tasks: 100,
      max_microtasks: 1000,
      max_wall_time: Some(Duration::from_millis(200)),
    })?;

    assert_eq!(
      get_global_prop_utf8(&mut host, "__msg_count").unwrap_or_default(),
      "0",
      "message handler should not have run after external queue overflow"
    );

    server.join().expect("server thread panicked");
    Ok(())
  }

  #[test]
  fn websocket_connect_timeout_emits_error_and_does_not_hang_teardown() -> Result<()> {
    let _lock = net_test_lock();
    // Use a reserved TEST-NET address that should not be reachable in CI. Without an explicit
    // connect timeout, some OS/network combinations can hang in `connect()` for a long time.
    let target = "ws://192.0.2.1:9/";

    // Phase 1: ensure the failed connection surfaces `error` + `close` events.
    let dom = dom2::Document::new(QuirksMode::NoQuirks);
    let mut host = make_host(dom, "http://example.invalid/")?;
    host.exec_script(&format!(
      r#"
      globalThis.__got_error = false;
      globalThis.__got_close = false;
      globalThis.__ws = new WebSocket({target:?});
      const ws = globalThis.__ws;
      ws.onerror = function () {{ globalThis.__got_error = true; }};
      ws.onclose = function () {{ globalThis.__got_close = true; }};
      "#,
    ))?;

    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
      let _ = host.run_until_idle(RunLimits {
        max_tasks: 100,
        max_microtasks: 1000,
        max_wall_time: Some(Duration::from_millis(50)),
      })?;

      let got_error = get_global_prop_utf8(&mut host, "__got_error").unwrap_or_default() == "true";
      let got_close = get_global_prop_utf8(&mut host, "__got_close").unwrap_or_default() == "true";
      if got_error && got_close {
        break;
      }
      if Instant::now() >= deadline {
        break;
      }
      std::thread::sleep(Duration::from_millis(10));
    }

    assert_eq!(
      get_global_prop_utf8(&mut host, "__got_error").as_deref(),
      Some("true"),
      "expected WebSocket error event to be observed",
    );
    assert_eq!(
      get_global_prop_utf8(&mut host, "__got_close").as_deref(),
      Some("true"),
      "expected WebSocket close event to be observed",
    );

    drop(host);

    // Phase 2: ensure env teardown (Drop) cannot hang joining the websocket thread even if the
    // thread is stuck in its connect path.
    let dom = dom2::Document::new(QuirksMode::NoQuirks);
    let mut host = make_host(dom, "http://example.invalid/")?;
    host.exec_script(&format!(
      r#"globalThis.__ws = new WebSocket({target:?});"#,
    ))?;
    let drop_start = Instant::now();
    drop(host);
    assert!(
      drop_start.elapsed() < Duration::from_secs(5),
      "websocket env teardown took too long (connect timeout not enforced?)",
    );

    Ok(())
  }

  #[test]
  fn websocket_teardown_does_not_hang_when_send_queue_is_full() -> Result<()> {
    let _lock = net_test_lock();
    let Some(listener) = try_bind_localhost("websocket_teardown_does_not_hang_when_send_queue_is_full") else {
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
            let mut ws = tungstenite::accept(stream).expect("accept websocket");
            let read_deadline = Instant::now() + Duration::from_secs(5);
            loop {
              match ws.read_message() {
                Ok(Message::Close(frame)) => {
                  let _ = ws.close(frame);
                  break;
                }
                Ok(Message::Ping(payload)) => {
                  let _ = ws.write_message(Message::Pong(payload));
                }
                Ok(_) => {}
                Err(tungstenite::Error::ConnectionClosed)
                | Err(tungstenite::Error::AlreadyClosed)
                | Err(tungstenite::Error::Protocol(_)) => break,
                Err(tungstenite::Error::Io(ref err))
                  if matches!(err.kind(), std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock) =>
                {
                  if Instant::now() >= read_deadline {
                    panic!("server did not observe client close");
                  }
                }
                Err(err) => panic!("server read failed: {err}"),
              }
            }
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

    let dom = dom2::Document::new(QuirksMode::NoQuirks);
    let mut host = make_host(dom, "http://example.invalid/")?;
    host.exec_script(&format!(
      r#"
      globalThis.__done = false;
      globalThis.__threw = false;
      globalThis.__throwMessage = "";
      globalThis.__ws = new WebSocket("ws://{addr}/");
      const ws = globalThis.__ws;
      ws.onopen = function () {{
        // Give the socket thread time to enter its read loop (it uses a short read timeout for
        // responsiveness). While it is blocked in read(), fill the bounded send queue.
        setTimeout(function () {{
          const msg = "x";
          for (let i = 0; i < {max}; i++) {{
            try {{
              ws.send(msg);
            }} catch (e) {{
              globalThis.__threw = true;
              globalThis.__throwMessage = String(e && e.message);
              break;
            }}
          }}
          globalThis.__done = true;
        }}, 10);
      }};
      "#,
      max = MAX_QUEUED_WEBSOCKET_SEND_COMMANDS + 32,
    ))?;

    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
      let _ = host.run_until_idle(RunLimits {
        max_tasks: 200,
        max_microtasks: 2000,
        max_wall_time: Some(Duration::from_millis(50)),
      })?;
      let done = get_global_prop_utf8(&mut host, "__done").unwrap_or_default() == "true";
      if done {
        break;
      }
      if Instant::now() >= deadline {
        break;
      }
      std::thread::sleep(Duration::from_millis(10));
    }

    assert_eq!(
      get_global_prop_utf8(&mut host, "__threw").as_deref(),
      Some("true"),
      "expected send() to throw once the bounded send queue is full; message={:?}",
      get_global_prop_utf8(&mut host, "__throwMessage")
    );

    let drop_start = Instant::now();
    drop(host);
    assert!(
      drop_start.elapsed() < Duration::from_secs(5),
      "websocket env teardown took too long (send-queue-full shutdown should be bounded)",
    );

    server.join().expect("server thread panicked");
    Ok(())
  }

  #[test]
  fn websocket_rejects_unrequested_protocol_selected_by_server() -> Result<()> {
    let _lock = net_test_lock();
    let Some(listener) = try_bind_localhost("websocket_rejects_unrequested_protocol_selected_by_server") else {
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

    let dom = dom2::Document::new(QuirksMode::NoQuirks);
    let mut host = make_host(dom, "http://example.invalid/")?;

    host.exec_script(&format!(
      r#"
      globalThis.__done = false;
      globalThis.__opened = false;
      globalThis.__error = false;
      globalThis.__closed = false;
      globalThis.__ws = new WebSocket("ws://{addr}/", ["chat"]);
      const ws = globalThis.__ws;
      ws.onopen = function () {{
        globalThis.__opened = true;
        globalThis.__done = true;
      }};
      ws.onerror = function () {{
        globalThis.__error = true;
      }};
      ws.onclose = function () {{
        globalThis.__closed = true;
        globalThis.__done = true;
      }};
      "#,
    ))?;

    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
      let _ = host.run_until_idle(RunLimits {
        max_tasks: 100,
        max_microtasks: 1000,
        max_wall_time: Some(Duration::from_millis(50)),
      })?;

      let done = get_global_prop_utf8(&mut host, "__done").unwrap_or_default();
      if done == "true" {
        break;
      }
      if Instant::now() >= deadline {
        break;
      }
      std::thread::sleep(Duration::from_millis(10));
    }

    assert_eq!(
      get_global_prop_utf8(&mut host, "__opened").as_deref(),
      Some("false"),
      "expected server-selected subprotocol to fail the connection",
    );
    assert_eq!(
      get_global_prop_utf8(&mut host, "__error").as_deref(),
      Some("true"),
      "expected websocket error when server selects invalid protocol",
    );
    assert_eq!(
      get_global_prop_utf8(&mut host, "__closed").as_deref(),
      Some("true"),
      "expected websocket close when server selects invalid protocol",
    );

    server.join().expect("server thread panicked");
    Ok(())
  }

  #[test]
  fn websocket_protocol_is_set_from_server_handshake_response() -> Result<()> {
    let _lock = net_test_lock();
    let Some(listener) = try_bind_localhost("websocket_protocol_is_set_from_server_handshake_response") else {
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
                .insert("Sec-WebSocket-Protocol", "superchat".parse().unwrap());
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

    let dom = dom2::Document::new(QuirksMode::NoQuirks);
    let mut host = make_host(dom, "http://example.invalid/")?;

    host.exec_script(&format!(
      r#"
       globalThis.__done = false;
       globalThis.__err = "";
       globalThis.__protocol = "";
       globalThis.__ws = new WebSocket("ws://{addr}/", ["chat", "superchat"]);
       const ws = globalThis.__ws;
       ws.onopen = function () {{
         globalThis.__protocol = ws.protocol;
         ws.close();
       }};
       ws.onerror = function () {{
         globalThis.__err = "error";
         globalThis.__done = true;
       }};
       ws.onclose = function () {{
         globalThis.__done = true;
       }};
       "#,
    ))?;

    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
      let _ = host.run_until_idle(RunLimits {
        max_tasks: 100,
        max_microtasks: 1000,
        max_wall_time: Some(Duration::from_millis(50)),
      })?;

      let done = get_global_prop_utf8(&mut host, "__done").unwrap_or_default();
      if done == "true" {
        break;
      }
      if Instant::now() >= deadline {
        break;
      }
      std::thread::sleep(Duration::from_millis(10));
    }

    assert_eq!(
      get_global_prop_utf8(&mut host, "__err").unwrap_or_default(),
      "",
      "unexpected websocket error: {:?}",
      get_global_prop_utf8(&mut host, "__err"),
    );
    assert_eq!(
      get_global_prop_utf8(&mut host, "__protocol").as_deref(),
      Some("superchat"),
    );

    server.join().expect("server thread panicked");
    Ok(())
  }

  #[test]
  fn websocket_rejects_protocol_when_none_were_requested() -> Result<()> {
    let _lock = net_test_lock();
    let Some(listener) = try_bind_localhost("websocket_rejects_protocol_when_none_were_requested") else {
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

    let dom = dom2::Document::new(QuirksMode::NoQuirks);
    let mut host = make_host(dom, "http://example.invalid/")?;

    host.exec_script(&format!(
      r#"
       globalThis.__done = false;
       globalThis.__opened = false;
       globalThis.__error = false;
       globalThis.__closed = false;
       globalThis.__ws = new WebSocket("ws://{addr}/");
       const ws = globalThis.__ws;
       ws.onopen = function () {{
         globalThis.__opened = true;
         globalThis.__done = true;
       }};
       ws.onerror = function () {{
         globalThis.__error = true;
       }};
       ws.onclose = function () {{
         globalThis.__closed = true;
         globalThis.__done = true;
       }};
       "#,
    ))?;

    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
      let _ = host.run_until_idle(RunLimits {
        max_tasks: 100,
        max_microtasks: 1000,
        max_wall_time: Some(Duration::from_millis(50)),
      })?;

      let done = get_global_prop_utf8(&mut host, "__done").unwrap_or_default();
      if done == "true" {
        break;
      }
      if Instant::now() >= deadline {
        break;
      }
      std::thread::sleep(Duration::from_millis(10));
    }

    assert_eq!(
      get_global_prop_utf8(&mut host, "__opened").as_deref(),
      Some("false"),
      "expected server-selected subprotocol to fail the connection",
    );
    assert_eq!(
      get_global_prop_utf8(&mut host, "__error").as_deref(),
      Some("true"),
      "expected websocket error when server selects protocol without request",
    );
    assert_eq!(
      get_global_prop_utf8(&mut host, "__closed").as_deref(),
      Some("true"),
      "expected websocket close when server selects protocol without request",
    );

    server.join().expect("server thread panicked");
    Ok(())
  }

  #[test]
  fn websocket_buffered_amount_cap() -> Result<()> {
    let _lock = net_test_lock();
    let Some(listener) = try_bind_localhost("websocket_buffered_amount_cap") else {
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
            // Keep the test deterministic (no indefinite blocking on slow/absent client I/O).
            let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
            let _ = stream.set_write_timeout(Some(Duration::from_secs(5)));
            let mut ws = tungstenite::accept(stream).expect("accept websocket");

            let read_deadline = Instant::now() + Duration::from_secs(5);
            loop {
              match ws.read_message() {
                Ok(tungstenite::Message::Close(frame)) => {
                  let _ = ws.close(frame);
                  break;
                }
                Ok(tungstenite::Message::Ping(payload)) => {
                  let _ = ws.write_message(tungstenite::Message::Pong(payload));
                }
                Ok(tungstenite::Message::Text(_)) | Ok(tungstenite::Message::Binary(_)) => {
                  // Discard; we're only exercising the client's send queue behaviour.
                }
                Ok(tungstenite::Message::Pong(_)) => {}
                Err(tungstenite::Error::Io(ref err))
                  if matches!(err.kind(), std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock) =>
                {
                  if Instant::now() >= read_deadline {
                    panic!("server read timed out");
                  }
                }
                Err(err) => panic!("server read failed: {err}"),
                _ => {}
              }
            }
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

    let dom = dom2::Document::new(QuirksMode::NoQuirks);
    let mut host = make_host(dom, "http://example.invalid/")?;

    let msg_size = MAX_WEBSOCKET_MESSAGE_BYTES;
    let cap = MAX_WEBSOCKET_BUFFERED_AMOUNT_BYTES;

    host.exec_script(&format!(
      r#"
      globalThis.__done = false;
      globalThis.__err = "";
      globalThis.__threw = false;
      globalThis.__throwName = "";
      globalThis.__throwMessage = "";
      globalThis.__bufferedAtThrow = 0;
      globalThis.__maxBuffered = 0;
      globalThis.__afterDrainOk = false;
      globalThis.__afterDrainError = "";
      globalThis.__closeBuffered = -1;

      globalThis.__ws = new WebSocket("ws://{addr}/");
      const ws = globalThis.__ws;
      const msg = new Uint8Array({msg_size});

      ws.onopen = function () {{
        for (let i = 0; i < 32; i++) {{
          try {{
            ws.send(msg);
            if (ws.bufferedAmount > globalThis.__maxBuffered) {{
              globalThis.__maxBuffered = ws.bufferedAmount;
            }}
          }} catch (e) {{
            globalThis.__threw = true;
            globalThis.__throwName = String(e && e.name);
            globalThis.__throwMessage = String(e && e.message);
            globalThis.__bufferedAtThrow = ws.bufferedAmount;
            break;
          }}
        }}

        if (!globalThis.__threw) {{
          globalThis.__err = "did_not_throw";
          ws.close();
          return;
        }}

        function poll() {{
          if (ws.bufferedAmount === 0) {{
            try {{
              ws.send(msg);
              globalThis.__afterDrainOk = true;
            }} catch (e) {{
              globalThis.__afterDrainOk = false;
              globalThis.__afterDrainError = String(e && e.message);
            }}
            ws.close();
            return;
          }}
          setTimeout(poll, 10);
        }}

        setTimeout(poll, 10);
      }};

      ws.onerror = function () {{
        globalThis.__err = "error";
        globalThis.__done = true;
      }};
      ws.onclose = function () {{
        globalThis.__closeBuffered = ws.bufferedAmount;
        globalThis.__done = true;
      }};
      "#,
    ))?;

    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
      let _ = host.run_until_idle(RunLimits {
        max_tasks: 200,
        max_microtasks: 2000,
        max_wall_time: Some(Duration::from_millis(50)),
      })?;

      let done = get_global_prop_utf8(&mut host, "__done").unwrap_or_default();
      if done == "true" {
        break;
      }
      if Instant::now() >= deadline {
        break;
      }
      std::thread::sleep(Duration::from_millis(10));
    }

    assert_eq!(
      get_global_prop_utf8(&mut host, "__err").unwrap_or_default(),
      "",
      "unexpected websocket error: {:?}",
      get_global_prop_utf8(&mut host, "__err")
    );

    assert_eq!(
      get_global_prop_utf8(&mut host, "__threw").unwrap_or_default(),
      "true",
      "expected send() to throw once bufferedAmount exceeds cap"
    );
    assert_eq!(
      get_global_prop_utf8(&mut host, "__throwName").as_deref(),
      Some("TypeError")
    );
    assert_eq!(
      get_global_prop_utf8(&mut host, "__throwMessage").as_deref(),
      Some("WebSocket bufferedAmount limit exceeded")
    );

    let max_buffered: f64 = get_global_prop_utf8(&mut host, "__maxBuffered")
      .unwrap_or_else(|| "0".to_string())
      .parse()
      .unwrap_or(0.0);
    let at_throw: f64 = get_global_prop_utf8(&mut host, "__bufferedAtThrow")
      .unwrap_or_else(|| "0".to_string())
      .parse()
      .unwrap_or(0.0);

    assert!(
      max_buffered <= cap as f64,
      "max bufferedAmount exceeded cap (max={max_buffered}, cap={cap})"
    );
    assert!(
      at_throw <= cap as f64,
      "bufferedAmount at throw exceeded cap (at_throw={at_throw}, cap={cap})"
    );

    assert_eq!(
      get_global_prop_utf8(&mut host, "__afterDrainOk").as_deref(),
      Some("true"),
      "expected send() to succeed after bufferedAmount drains; error={:?}",
      get_global_prop_utf8(&mut host, "__afterDrainError")
    );

    assert_eq!(
      get_global_prop_utf8(&mut host, "__closeBuffered").as_deref(),
      Some("0"),
      "expected bufferedAmount to be reset to 0 on close"
    );

    server.join().expect("server thread panicked");
    Ok(())
  }

  #[test]
  fn websocket_closes_when_event_queue_overflows() -> Result<()> {
    let _lock = net_test_lock();
    let Some(listener) = try_bind_localhost("websocket_closes_when_event_queue_overflows") else {
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
            // Bounded failures instead of hanging forever.
            let _ = stream.set_read_timeout(Some(Duration::from_millis(50)));
            let _ = stream.set_write_timeout(Some(Duration::from_secs(5)));
            let mut ws = tungstenite::accept(stream).expect("accept websocket");

            // Flood the client with many small messages to exceed
            // `MAX_QUEUED_WEBSOCKET_EVENTS_PER_SOCKET` (overridden to a very low value in tests).
            for _ in 0..512 {
              if ws.write_message(Message::Text("x".to_string())).is_err() {
                break;
              }
            }

            // Keep the connection open until the client initiates close.
            let read_deadline = Instant::now() + Duration::from_secs(5);
            loop {
              match ws.read_message() {
                Ok(Message::Close(_)) => break,
                Ok(_) => {}
                Err(tungstenite::Error::ConnectionClosed)
                | Err(tungstenite::Error::AlreadyClosed)
                | Err(tungstenite::Error::Protocol(_)) => break,
                Err(tungstenite::Error::Io(ref err))
                  if matches!(err.kind(), std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock) =>
                {
                  if Instant::now() >= read_deadline {
                    panic!("server did not observe client close");
                  }
                }
                Err(err) => panic!("server read failed: {err}"),
              }
            }
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

    let dom = dom2::Document::new(QuirksMode::NoQuirks);
    let mut host = make_host(dom, "http://example.invalid/")?;

    host.exec_script(&format!(
      r#"
      globalThis.__done = false;
      globalThis.__err = "";
      globalThis.__close_fired = false;
      globalThis.__close_code = 0;
      globalThis.__close_reason = "";
      globalThis.__ready_state = -1;
      globalThis.__ws = new WebSocket("ws://{addr}/");
      const ws = globalThis.__ws;
      ws.onmessage = function (_e) {{}};
      ws.onerror = function (_e) {{
        globalThis.__err = "error";
        globalThis.__done = true;
      }};
      ws.onclose = function (e) {{
        globalThis.__close_fired = true;
        globalThis.__close_code = Number(e && e.code) || 0;
        globalThis.__close_reason = String(e && e.reason);
        globalThis.__ready_state = ws.readyState;
        globalThis.__done = true;
      }};
      "#,
    ))?;

    // Allow the socket thread to queue enough events to overflow before we begin draining the JS
    // event loop.
    std::thread::sleep(Duration::from_millis(100));

    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
      let _ = host.run_until_idle(RunLimits {
        max_tasks: 100,
        max_microtasks: 1000,
        max_wall_time: Some(Duration::from_millis(50)),
      })?;

      let done = get_global_prop_utf8(&mut host, "__done").unwrap_or_default();
      if done == "true" {
        break;
      }
      if Instant::now() >= deadline {
        break;
      }
      std::thread::sleep(Duration::from_millis(10));
    }

    let expected_code = WS_CLOSE_EVENT_QUEUE_OVERFLOW_CODE.to_string();
    assert_eq!(
      get_global_prop_utf8(&mut host, "__err").unwrap_or_default(),
      "",
      "unexpected websocket error: {:?}",
      get_global_prop_utf8(&mut host, "__err")
    );
    assert_eq!(
      get_global_prop_utf8(&mut host, "__close_fired").as_deref(),
      Some("true"),
      "expected onclose to fire when the event queue overflows"
    );
    assert_eq!(
      get_global_prop_utf8(&mut host, "__ready_state").as_deref(),
      Some("3"),
      "expected websocket to transition to CLOSED"
    );
    assert_eq!(
      get_global_prop_utf8(&mut host, "__close_code").as_deref(),
      Some(expected_code.as_str()),
      "expected overflow close code"
    );
    assert_eq!(
      get_global_prop_utf8(&mut host, "__close_reason").as_deref(),
      Some(WS_CLOSE_EVENT_QUEUE_OVERFLOW_REASON),
      "expected overflow close reason"
    );

    server.join().expect("server thread panicked");
    Ok(())
  }

  #[test]
  fn websocket_closes_when_pending_event_bytes_overflow() -> Result<()> {
    let _lock = net_test_lock();
    let Some(listener) = try_bind_localhost("websocket_closes_when_pending_event_bytes_overflow") else {
      return Ok(());
    };
    listener.set_nonblocking(true).expect("set_nonblocking");
    let addr = listener.local_addr().expect("local_addr");

    // Ensure we trigger the pending-byte cap without hitting the (test-only) pending event-count cap.
    // In tests, `MAX_QUEUED_WEBSOCKET_EVENTS_PER_SOCKET` is 4, and the open + first message events are
    // below that limit.
    let msg_len = (MAX_WEBSOCKET_PENDING_EVENT_BYTES / 2).saturating_add(1);
    assert!(
      msg_len <= MAX_WEBSOCKET_MESSAGE_BYTES as usize,
      "test message len must be <= MAX_WEBSOCKET_MESSAGE_BYTES"
    );

    let server = std::thread::spawn(move || {
      let deadline = Instant::now() + Duration::from_secs(5);
      loop {
        match listener.accept() {
          Ok((stream, _)) => {
            let mut stream = stream;
            // Bounded failures instead of hanging forever.
            let _ = stream.set_read_timeout(Some(Duration::from_millis(50)));
            let _ = stream.set_write_timeout(Some(Duration::from_secs(5)));
            let mut ws = tungstenite::accept(stream).expect("accept websocket");

            let payload = vec![0u8; msg_len];
            // Two messages are enough to exceed `MAX_WEBSOCKET_PENDING_EVENT_BYTES`.
            for _ in 0..2 {
              if ws
                .write_message(Message::Binary(payload.clone()))
                .is_err()
              {
                break;
              }
            }

            // Keep the connection open until the client initiates close.
            let read_deadline = Instant::now() + Duration::from_secs(5);
            loop {
              match ws.read_message() {
                Ok(Message::Close(_)) => break,
                Ok(_) => {}
                Err(tungstenite::Error::ConnectionClosed)
                | Err(tungstenite::Error::AlreadyClosed)
                | Err(tungstenite::Error::Protocol(_)) => break,
                Err(tungstenite::Error::Io(ref err))
                  if matches!(err.kind(), std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock) =>
                {
                  if Instant::now() >= read_deadline {
                    panic!("server did not observe client close");
                  }
                }
                Err(err) => panic!("server read failed: {err}"),
              }
            }
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

    let dom = dom2::Document::new(QuirksMode::NoQuirks);
    let mut host = make_host(dom, "http://example.invalid/")?;

    host.exec_script(&format!(
      r#"
      globalThis.__done = false;
      globalThis.__err = "";
      globalThis.__close_fired = false;
      globalThis.__close_code = 0;
      globalThis.__close_reason = "";
      globalThis.__ready_state = -1;
      globalThis.__ws = new WebSocket("ws://{addr}/");
      const ws = globalThis.__ws;
      ws.onmessage = function (_e) {{}};
      ws.onerror = function (_e) {{
        globalThis.__err = "error";
        globalThis.__done = true;
      }};
      ws.onclose = function (e) {{
        globalThis.__close_fired = true;
        globalThis.__close_code = Number(e && e.code) || 0;
        globalThis.__close_reason = String(e && e.reason);
        globalThis.__ready_state = ws.readyState;
        globalThis.__done = true;
      }};
      "#,
    ))?;

    // Allow the socket thread to enqueue enough payload bytes to overflow before we begin draining
    // the JS event loop.
    std::thread::sleep(Duration::from_millis(100));

    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
      let _ = host.run_until_idle(RunLimits {
        max_tasks: 100,
        max_microtasks: 1000,
        max_wall_time: Some(Duration::from_millis(50)),
      })?;

      let done = get_global_prop_utf8(&mut host, "__done").unwrap_or_default();
      if done == "true" {
        break;
      }
      if Instant::now() >= deadline {
        break;
      }
      std::thread::sleep(Duration::from_millis(10));
    }

    let expected_code = WS_CLOSE_EVENT_QUEUE_OVERFLOW_CODE.to_string();
    assert_eq!(
      get_global_prop_utf8(&mut host, "__err").unwrap_or_default(),
      "",
      "unexpected websocket error: {:?}",
      get_global_prop_utf8(&mut host, "__err")
    );
    assert_eq!(
      get_global_prop_utf8(&mut host, "__close_fired").as_deref(),
      Some("true"),
      "expected onclose to fire when the pending-byte cap is exceeded"
    );
    assert_eq!(
      get_global_prop_utf8(&mut host, "__ready_state").as_deref(),
      Some("3"),
      "expected websocket to transition to CLOSED"
    );
    assert_eq!(
      get_global_prop_utf8(&mut host, "__close_code").as_deref(),
      Some(expected_code.as_str()),
      "expected overflow close code"
    );
    assert_eq!(
      get_global_prop_utf8(&mut host, "__close_reason").as_deref(),
      Some(WS_CLOSE_EVENT_QUEUE_OVERFLOW_REASON),
      "expected overflow close reason"
    );

    server.join().expect("server thread panicked");
    Ok(())
  }

  #[test]
  fn websocket_send_oversize_uint8array_throws_without_buffering() -> Result<()> {
    let _lock = net_test_lock();
    let Some(listener) =
      try_bind_localhost("websocket_send_oversize_uint8array_throws_without_buffering")
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
            let mut ws = tungstenite::accept(stream).expect("accept websocket");

            let read_deadline = Instant::now() + Duration::from_secs(5);
            loop {
              match ws.read_message() {
                Ok(tungstenite::Message::Close(frame)) => {
                  let _ = ws.close(frame);
                  break;
                }
                Ok(tungstenite::Message::Ping(payload)) => {
                  let _ = ws.write_message(tungstenite::Message::Pong(payload));
                }
                Ok(_) => {}
                Err(tungstenite::Error::Io(ref err))
                  if matches!(
                    err.kind(),
                    std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
                  ) =>
                {
                  if Instant::now() >= read_deadline {
                    panic!("server read timed out");
                  }
                }
                Err(err) => panic!("server read failed: {err}"),
              }
            }
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

    let dom = dom2::Document::new(QuirksMode::NoQuirks);
    // Use an insecure document URL so `ws://` is allowed (secure contexts block mixed content).
    let mut host = make_host(dom, "http://example.invalid/")?;

    let oversize = MAX_WEBSOCKET_MESSAGE_BYTES + 1;

    host.exec_script(&format!(
      r#"
      globalThis.__done = false;
      globalThis.__err = "";
      globalThis.__threw = false;
      globalThis.__throwName = "";
      globalThis.__throwMessage = "";
      globalThis.__bufferedAtThrow = -1;

      const ws = new WebSocket("ws://{addr}/");
      ws.onopen = function () {{
        const msg = new Uint8Array({oversize});
        try {{
          ws.send(msg);
          globalThis.__err = "did_not_throw";
        }} catch (e) {{
          globalThis.__threw = true;
          globalThis.__throwName = String(e && e.name);
          globalThis.__throwMessage = String(e && e.message);
          globalThis.__bufferedAtThrow = ws.bufferedAmount;
        }}
        ws.close();
      }};
      ws.onerror = function () {{
        globalThis.__err = "error";
        globalThis.__done = true;
      }};
      ws.onclose = function () {{
        globalThis.__done = true;
      }};
      "#,
    ))?;

    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
      let _ = host.run_until_idle(RunLimits {
        max_tasks: 200,
        max_microtasks: 2000,
        max_wall_time: Some(Duration::from_millis(50)),
      })?;

      let done = get_global_prop_utf8(&mut host, "__done").unwrap_or_default();
      if done == "true" {
        break;
      }
      if Instant::now() >= deadline {
        break;
      }
      std::thread::sleep(Duration::from_millis(10));
    }

    assert_eq!(
      get_global_prop_utf8(&mut host, "__err").unwrap_or_default(),
      "",
      "unexpected websocket error: {:?}",
      get_global_prop_utf8(&mut host, "__err")
    );
    assert_eq!(
      get_global_prop_utf8(&mut host, "__threw").as_deref(),
      Some("true")
    );
    assert_eq!(
      get_global_prop_utf8(&mut host, "__throwName").as_deref(),
      Some("TypeError")
    );
    assert_eq!(
      get_global_prop_utf8(&mut host, "__throwMessage").as_deref(),
      Some("WebSocket message too large")
    );
    assert_eq!(
      get_global_prop_utf8(&mut host, "__bufferedAtThrow").as_deref(),
      Some("0"),
      "expected bufferedAmount to remain 0 after rejecting oversize send"
    );

    server.join().expect("server thread panicked");
    Ok(())
  }

  fn websocket_close_code_test(code_expr: &str, expect_throw: bool) -> Result<()> {
    let _lock = net_test_lock();
    let Some(listener) = try_bind_localhost("websocket_close_code_test") else {
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
            let mut ws = tungstenite::accept(stream).expect("accept websocket");

            // Wait for the client to close.
            let read_deadline = Instant::now() + Duration::from_secs(5);
            loop {
              match ws.read_message() {
                Ok(Message::Close(frame)) => {
                  // Reply close.
                  let _ = ws.close(frame);
                  break;
                }
                Ok(_) => {}
                Err(tungstenite::Error::Io(ref err))
                  if matches!(
                    err.kind(),
                    std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
                  ) =>
                {
                  if Instant::now() >= read_deadline {
                    panic!("server read timed out");
                  }
                }
                Err(tungstenite::Error::ConnectionClosed) => break,
                Err(err) => panic!("server read failed: {err}"),
              }
            }

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

    let dom = dom2::Document::new(QuirksMode::NoQuirks);
    let mut host = make_host(dom, "http://example.invalid/")?;

    host.exec_script(&format!(
      r#"
      globalThis.__done = false;
      globalThis.__ws_error = "";
      globalThis.__threw = false;
      globalThis.__throw_name = "";
      globalThis.__state_after = -1;
      globalThis.__ws = new WebSocket("ws://{addr}/");
      const ws = globalThis.__ws;
      ws.onopen = function () {{
        try {{
          ws.close({code_expr});
        }} catch (e) {{
          globalThis.__threw = true;
          globalThis.__throw_name = String(e && e.name);
        }}
        globalThis.__state_after = ws.readyState;
        // Ensure the connection is closed so the test doesn't leak threads even if the call threw.
        if (globalThis.__threw) {{
          ws.close(1000);
        }}
      }};
      ws.onerror = function () {{
        globalThis.__ws_error = "error";
        globalThis.__done = true;
      }};
      ws.onclose = function () {{
        globalThis.__done = true;
      }};
      "#,
    ))?;

    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
      let _ = host.run_until_idle(RunLimits {
        max_tasks: 100,
        max_microtasks: 1000,
        max_wall_time: Some(Duration::from_millis(50)),
      })?;

      let done = get_global_prop_utf8(&mut host, "__done").unwrap_or_default();
      if done == "true" {
        break;
      }
      if Instant::now() >= deadline {
        break;
      }
      std::thread::sleep(Duration::from_millis(10));
    }

    assert_eq!(
      get_global_prop_utf8(&mut host, "__done").unwrap_or_default(),
      "true",
      "websocket test timed out"
    );

    assert_eq!(
      get_global_prop_utf8(&mut host, "__ws_error").unwrap_or_default(),
      "",
      "unexpected websocket error"
    );

    assert_eq!(
      get_global_prop_utf8(&mut host, "__threw").unwrap_or_default(),
      expect_throw.to_string()
    );

    let throw_name = get_global_prop_utf8(&mut host, "__throw_name").unwrap_or_default();
    if expect_throw {
      assert_eq!(throw_name, "TypeError");
    } else {
      assert_eq!(throw_name, "");
    }

    let state_after = get_global_prop_utf8(&mut host, "__state_after").unwrap_or_default();
    if expect_throw {
      assert_eq!(
        state_after,
        WS_OPEN.to_string(),
        "readyState changed after invalid close"
      );
    } else {
      assert_eq!(state_after, WS_CLOSING.to_string());
    }

    server.join().expect("server thread panicked");
    Ok(())
  }

  #[test]
  fn websocket_entries_removed_after_close_and_properties_tombstoned() -> Result<()> {
    let _lock = net_test_lock();
    // Pick a port that is very likely to be closed so connections fail quickly without requiring us
    // to run a WebSocket server.
    let Some(listener) = try_bind_localhost("websocket_entries_removed_after_close_and_properties_tombstoned") else {
      return Ok(());
    };
    let port = listener.local_addr().expect("local_addr").port();
    drop(listener);

    let dom = dom2::Document::new(QuirksMode::NoQuirks);
    // Use an insecure document URL so `ws://` is allowed (mixed content is blocked for secure
    // contexts).
    let mut host = make_host(dom, "http://example.invalid/__ws_cleanup_test")?;

    let mut env_id: u64 = 0;
    let resolved_url = format!("ws://127.0.0.1:{port}/");

    for _ in 0..64 {
      host.exec_script(&format!(
        r#"
        globalThis.__done = false;
        globalThis.__ws = new WebSocket({resolved_url:?});
        globalThis.__env_id = globalThis.__ws.__fastrender_websocket_env_id;
        globalThis.__ws.onerror = function () {{}};
        globalThis.__ws.onclose = function () {{
          globalThis.__done = true;
        }};
        "#,
      ))?;

      let deadline = Instant::now() + Duration::from_secs(5);
      loop {
        let _ = host.run_until_idle(RunLimits {
          max_tasks: 100,
          max_microtasks: 1000,
          max_wall_time: Some(Duration::from_millis(50)),
        })?;

        if get_global_prop_utf8(&mut host, "__done").as_deref() == Some("true") {
          break;
        }
        if Instant::now() >= deadline {
          panic!("websocket close timed out");
        }
        std::thread::sleep(Duration::from_millis(10));
      }

      if env_id == 0 {
        let raw = get_global_prop_utf8(&mut host, "__env_id").unwrap_or_default();
        env_id = raw.parse::<f64>().unwrap_or(0.0) as u64;
        assert!(env_id > 0, "env id should be set");
      }

      // The close task has finished (including cleanup). The Rust entry should be removed while JS
      // accessors continue to work via tombstone properties.
      host.exec_script(
        r#"
        globalThis.__ready = globalThis.__ws.readyState;
        globalThis.__url = globalThis.__ws.url;
        globalThis.__protocol = globalThis.__ws.protocol;
        globalThis.__buffered = globalThis.__ws.bufferedAmount;
        "#,
      )?;

      let closed = (WS_CLOSED as f64).to_string();
      assert_eq!(
        get_global_prop_utf8(&mut host, "__ready").as_deref(),
        Some(closed.as_str())
      );
      assert_eq!(
        get_global_prop_utf8(&mut host, "__url").as_deref(),
        Some(resolved_url.as_str())
      );
      assert_eq!(
        get_global_prop_utf8(&mut host, "__protocol").as_deref(),
        Some("")
      );
      assert_eq!(
        get_global_prop_utf8(&mut host, "__buffered").as_deref(),
        Some("0")
      );

      let sockets_len = {
        let lock = envs().lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        lock
          .get(&env_id)
          .expect("env should still be registered")
          .sockets
          .len()
      };
      assert_eq!(sockets_len, 0, "socket entry should be removed after close");

      host.exec_script("globalThis.__ws = null;")?;
    }

    Ok(())
  }

  #[test]
  fn websocket_close_code_1000_ok() -> Result<()> {
    websocket_close_code_test("1000", false)
  }

  #[test]
  fn websocket_close_code_1006_throws() -> Result<()> {
    websocket_close_code_test("1006", true)
  }

  #[test]
  fn websocket_close_code_negative_throws() -> Result<()> {
    websocket_close_code_test("-1", true)
  }

  #[test]
  fn websocket_close_code_out_of_range_throws() -> Result<()> {
    websocket_close_code_test("70000", true)
  }

  #[test]
  fn websocket_blocks_mixed_content_from_secure_context() -> Result<()> {
    let dom = dom2::Document::new(QuirksMode::NoQuirks);
    let mut host = make_host(dom, "https://example.invalid/")?;

    let err = host
      .exec_script("new WebSocket('ws://127.0.0.1')")
      .expect_err("expected mixed content ws:// to be blocked");
    let msg = err.to_string();
    assert!(
      msg.contains("Mixed content"),
      "expected error to mention mixed content, got: {msg:?}"
    );
    Ok(())
  }

  #[test]
  fn websocket_allows_wss_from_secure_context() -> Result<()> {
    let dom = dom2::Document::new(QuirksMode::NoQuirks);
    let mut host = make_host(dom, "https://example.invalid/")?;

    // Connection may fail (no server), but the constructor should not throw.
    // Use an IP + unlikely-to-be-listening port to ensure the background connect exits quickly.
    host.exec_script("globalThis.__ws = new WebSocket('wss://127.0.0.1:1/')")?;
    Ok(())
  }

  #[test]
  fn websocket_thread_blocks_ws_when_document_is_secure() {
    let fetcher: Arc<dyn ResourceFetcher> = Arc::new(HttpFetcher::new());
    let env_id = NEXT_ENV_ID.fetch_add(1, Ordering::Relaxed);
    {
      let mut lock = envs().lock().unwrap_or_else(|poisoned| poisoned.into_inner());
      lock.insert(
        env_id,
        EnvState::new(WindowWebSocketEnv::for_document(fetcher.clone(), None), RealmId::from_raw(0)),
      );
    }

    let (cmd_tx, cmd_rx) = mpsc::sync_channel::<WsCommand>(1);

    // Use a standalone heap allocation for the weak handle. The queued tasks won't actually run in
    // this test; we only assert that the handler queued Error+Close events without panicking.
    let mut heap = vm_js::Heap::new(vm_js::HeapLimits::new(1024 * 1024, 512 * 1024));
    let mut scope = heap.scope();
    let ws_obj = scope.alloc_object().expect("alloc websocket object");
    let weak = WeakGcObject::from(ws_obj);

    let ws_id = with_env_state_mut(env_id, |state| {
      let id = state.alloc_id();
      state.sockets.insert(
        id,
        WebSocketState {
          weak_obj: weak,
          url: String::new(),
          requested_protocols: Vec::new(),
          protocol: String::new(),
          ready_state: WS_CONNECTING,
          binary_type: WebSocketBinaryType::default(),
          buffered_amount: 0,
          pending_events: 0,
          pending_event_bytes: 0,
          close_task_queued: false,
          forced_close: None,
          cmd_tx: Some(cmd_tx),
          thread: None,
        },
      );
      Ok(id)
    })
    .expect("register websocket state");

    let event_loop = EventLoop::<WindowHostState>::new();
    let task_queue = event_loop.external_task_queue_handle();

    websocket_thread_main::<WindowHostState>(
      env_id,
      ws_id,
      fetcher,
      "ws://127.0.0.1:1/".to_string(),
      true,
      Vec::new(),
      cmd_rx,
      task_queue,
    );

    with_env_state(env_id, |state| {
      let ws = state
        .sockets
        .get(&ws_id)
        .ok_or(VmError::InvariantViolation("missing websocket state"))?;
      assert_eq!(ws.ready_state, WS_CLOSED);
      // Should have queued a task that dispatches `error` + `close` events.
      assert_eq!(ws.pending_events, 1);
      Ok(())
    })
    .expect("inspect websocket state");

    unregister_window_websocket_env(env_id);
  }

  #[test]
  fn websocket_ipc_connect_rejects_invalid_url_without_panic() {
    // Create a minimal env/socket entry so the "network process" connect handler can queue events
    // on failure. This simulates a compromised renderer sending an invalid URL over IPC.
    let fetcher: Arc<dyn ResourceFetcher> = Arc::new(HttpFetcher::new());
    let env_id = NEXT_ENV_ID.fetch_add(1, Ordering::Relaxed);
    {
      let mut lock = envs().lock().unwrap_or_else(|poisoned| poisoned.into_inner());
      lock.insert(
        env_id,
        EnvState::new(WindowWebSocketEnv::for_document(fetcher.clone(), None), RealmId::from_raw(0)),
      );
    }

    let (cmd_tx, cmd_rx) = mpsc::sync_channel::<WsCommand>(1);

    // Use a standalone heap allocation for the weak handle. The queued tasks won't actually run in
    // this test; we only assert that the handler queued Error+Close events without panicking.
    let mut heap = vm_js::Heap::new(vm_js::HeapLimits::new(1024 * 1024, 512 * 1024));
    let mut scope = heap.scope();
    let ws_obj = scope.alloc_object().expect("alloc websocket object");
    let weak = WeakGcObject::from(ws_obj);

    let ws_id = with_env_state_mut(env_id, |state| {
      let id = state.alloc_id();
      state.sockets.insert(
        id,
        WebSocketState {
          weak_obj: weak,
          url: String::new(),
          requested_protocols: Vec::new(),
          protocol: String::new(),
          ready_state: WS_CONNECTING,
          binary_type: WebSocketBinaryType::default(),
          buffered_amount: 0,
          pending_events: 0,
          pending_event_bytes: 0,
          close_task_queued: false,
          forced_close: None,
          cmd_tx: Some(cmd_tx),
          thread: None,
        },
      );
      Ok(id)
    })
    .expect("register websocket state");

    let event_loop = EventLoop::<WindowHostState>::new();
    let task_queue = event_loop.external_task_queue_handle();

    // Missing-host URL that a compromised renderer might send.
    websocket_thread_main::<WindowHostState>(
      env_id,
      ws_id,
      fetcher,
      "ws:/relative".to_string(),
      false,
      Vec::new(),
      cmd_rx,
      task_queue,
    );

    with_env_state(env_id, |state| {
      let ws = state
        .sockets
        .get(&ws_id)
        .ok_or(VmError::InvariantViolation("missing websocket state"))?;
      assert_eq!(ws.ready_state, WS_CLOSED);
      // Should have queued a task that dispatches `error` + `close` events.
      assert_eq!(ws.pending_events, 1);
      Ok(())
    })
    .expect("inspect websocket state");

    unregister_window_websocket_env(env_id);
  }

  #[test]
  fn websocket_constructor_enforces_env_cap() -> Result<()> {
    let dom = dom2::Document::new(QuirksMode::NoQuirks);
    let document_url = "https://example.invalid/__ws_cap_test";
    let mut host = make_host(dom, document_url)?;

    let env_id = {
      let lock = envs().lock().unwrap_or_else(|poisoned| poisoned.into_inner());
      lock
        .iter()
        .find_map(|(id, state)| {
          (state.env.document_url.as_deref() == Some(document_url)).then_some(*id)
        })
        .expect("websocket env should be registered")
    };

    // Use a stable, rooted JS object (the global object) so sweep does not remove the dummy
    // entries.
    let global_obj = {
      let window = host.host_mut().window_mut();
      let (_vm, realm, _heap) = window.vm_realm_and_heap_mut();
      realm.global_object()
    };

    let (cmd_tx, _cmd_rx) = mpsc::sync_channel::<WsCommand>(1);

    with_env_state_mut(env_id, |state| {
      state.sockets.clear();
      state.next_id = 1;
      for _ in 0..MAX_WEBSOCKETS_PER_ENV {
        let id = state.alloc_id();
        state.sockets.insert(
          id,
         WebSocketState {
            weak_obj: WeakGcObject::from(global_obj),
            url: "ws://example.invalid/".to_string(),
            requested_protocols: Vec::new(),
            protocol: String::new(),
            ready_state: WS_CONNECTING,
            buffered_amount: 0,
            pending_events: 0,
            close_task_queued: false,
            forced_close: None,
            cmd_tx: Some(cmd_tx.clone()),
            thread: None,
          },
        );
      }
      Ok(())
    })
    .unwrap();

    host.exec_script(
      r#"
      globalThis.__cap_err = "";
      try {
        new WebSocket("wss://127.0.0.1:1/");
      } catch (e) {
        globalThis.__cap_err = String(e && e.message || "");
      }
      "#,
    )?;

    assert_eq!(
      get_global_prop_utf8(&mut host, "__cap_err").as_deref(),
      Some("Too many WebSocket connections")
    );

    Ok(())
  }

  #[test]
  fn websocket_ipc_env_unregister_sends_shutdown_for_all_sockets() {
    let fetcher: Arc<dyn ResourceFetcher> = Arc::new(HttpFetcher::new());
    let env_id = NEXT_ENV_ID.fetch_add(1, Ordering::Relaxed);

    let (cmd_tx, cmd_rx) = mpsc::sync_channel::<WebSocketIpcCommand>(16);
    let (_event_tx, event_rx) = mpsc::channel::<WebSocketIpcEvent>();
    let ipc_state = IpcEnvState {
      cmd_tx,
      event_rx: Some(event_rx),
      stop: Arc::new(AtomicBool::new(false)),
      thread: None,
    };

    {
      let mut lock = envs().lock().unwrap_or_else(|poisoned| poisoned.into_inner());
      lock.insert(env_id, EnvState::new_ipc(WindowWebSocketEnv::for_document(fetcher, None), ipc_state));
    }

    // Register a few fake sockets.
    let mut heap = vm_js::Heap::new(vm_js::HeapLimits::new(1024 * 1024, 512 * 1024));
    let mut scope = heap.scope();
    let ws_obj = scope.alloc_object().expect("alloc ws object");
    let weak = WeakGcObject::from(ws_obj);
    with_env_state_mut(env_id, |state| {
      state.next_id = 1;
      for _ in 0..3 {
        let id = state.alloc_id();
        state.sockets.insert(
          id,
          WebSocketState {
            weak_obj: weak,
            url: "ws://example.invalid/".to_string(),
            requested_protocols: Vec::new(),
            protocol: String::new(),
            ready_state: WS_OPEN,
            buffered_amount: 0,
            pending_events: 0,
            close_task_queued: false,
            forced_close: None,
            cmd_tx: None,
            thread: None,
          },
        );
      }
      Ok(())
    })
    .unwrap();

    unregister_window_websocket_env(env_id);

    let mut conn_ids: Vec<u64> = Vec::new();
    for _ in 0..3 {
      match cmd_rx.recv_timeout(Duration::from_secs(1)) {
        Ok(WebSocketIpcCommand::WebSocket { conn_id, cmd }) => {
          assert_eq!(cmd, WebSocketCommand::Shutdown);
          conn_ids.push(conn_id);
        }
        other => panic!("unexpected IPC command after env unregister: {other:?}"),
      }
    }
    conn_ids.sort_unstable();
    assert_eq!(conn_ids, vec![1, 2, 3]);
  }
} 
