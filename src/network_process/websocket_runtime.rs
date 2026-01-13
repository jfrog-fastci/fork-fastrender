//! WebSocket implementation intended to live in the (trusted) network process.
//!
//! The renderer process is untrusted: it must not perform `tungstenite::connect` or any socket I/O.
//! Instead, the renderer sends [`crate::ipc::network::RendererToNetwork`] commands and receives
//! [`crate::ipc::network::NetworkToRenderer`] events.
//!
//! This module provides an in-process reference implementation that mirrors the intended
//! multi-process behavior. It is used by tests and as scaffolding while the network process is
//! being built out.
//!
//! Design:
//! - A bounded Tokio runtime drives all WebSocket I/O.
//! - Each connection is a Tokio task (not an OS thread), keeping the network process scalable even
//!   when a compromised renderer opens many sockets.
//! - All renderer-supplied commands are validated via [`crate::ipc::websocket::WebSocketCommand`].
//! - Network → renderer events are queued per-socket with a hard cap to prevent unbounded memory
//!   growth (backpressure). On overflow (count or bytes), the socket is closed and an error is sent.

use crate::ipc::network::{NetworkToRenderer, RendererToNetwork};
use crate::ipc::websocket::{
  WebSocketCommand, WebSocketConnectParams, WebSocketEvent, WebSocketValidationError,
  MAX_WEBSOCKET_CLOSE_REASON_BYTES, MAX_WEBSOCKET_MESSAGE_BYTES,
};
use std::borrow::Cow;
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;
use std::sync::OnceLock;
use std::thread::JoinHandle;
use std::time::Duration;

use futures_util::{SinkExt as _, StreamExt as _};
use tokio::sync::mpsc as tokio_mpsc;
use tokio::time::timeout;
use tungstenite::client::IntoClientRequest;
use tungstenite::protocol::{CloseFrame, Message};

const WS_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const WS_WRITE_TIMEOUT: Duration = Duration::from_secs(10);
const WS_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(2);

// Tokio runtime sizing: keep strictly bounded so many connections cannot translate into many OS
// threads. These defaults are conservative; this module is primarily I/O bound.
const WS_IO_WORKER_THREADS: usize = 4;
const WS_IO_MAX_BLOCKING_THREADS: usize = 4;

/// Matches `window_websocket.rs`'s `MAX_QUEUED_WEBSOCKET_EVENTS_PER_SOCKET`.
const MAX_QUEUED_WEBSOCKET_EVENTS_PER_SOCKET: usize = 1024;
/// Matches `window_websocket.rs`'s `MAX_QUEUED_WEBSOCKET_SEND_COMMANDS`.
const MAX_QUEUED_WEBSOCKET_SEND_COMMANDS: usize = 1024;
/// Hard cap on total queued inbound WebSocket message payload bytes per socket.
///
/// This prevents a compromised renderer or hostile server from causing multi‑GiB accumulation in
/// the network process when the renderer is not draining its IPC event stream.
#[cfg(not(test))]
const MAX_WEBSOCKET_PENDING_EVENT_BYTES: usize = 32 * 1024 * 1024;
// Keep overflow tests deterministic with a much smaller limit.
#[cfg(test)]
const MAX_WEBSOCKET_PENDING_EVENT_BYTES: usize = 512 * 1024;

const CLOSE_CODE_POLICY_VIOLATION: u16 = 1008;
const CLOSE_CODE_MESSAGE_TOO_BIG: u16 = 1009;
const CLOSE_CODE_ABNORMAL: u16 = 1006;
const CLOSE_CODE_GOING_AWAY: u16 = 1001;

#[derive(Debug)]
enum WsCommand {
  SendText(String),
  SendBinary(Vec<u8>),
  Close {
    code: Option<u16>,
    reason: Option<String>,
  },
  Shutdown,
}

#[derive(Debug)]
enum WsThreadMsg {
  Event {
    conn_id: u64,
    generation: u64,
    event: WebSocketEvent,
  },
}

struct SocketEntry {
  generation: u64,
  cmd_tx: Option<tokio_mpsc::Sender<WsCommand>>,
  task: Option<tokio::task::JoinHandle<()>>,
  pending_events: VecDeque<WebSocketEvent>,
  pending_event_bytes: usize,
  overflowed: bool,
}

/// Handle to a running in-process "network process" WebSocket runtime.
///
/// Dropping the handle shuts down the network thread and all active connections (best-effort).
pub struct WebSocketNetworkProcessHandle {
  cmd_tx: Option<mpsc::SyncSender<RendererToNetwork>>,
  event_rx: mpsc::Receiver<NetworkToRenderer>,
  join: Option<JoinHandle<()>>,
}

impl WebSocketNetworkProcessHandle {
  /// Send a command to the network process.
  pub fn send(&self, cmd: RendererToNetwork) -> Result<(), String> {
    self
      .cmd_tx
      .as_ref()
      .ok_or_else(|| "network process disconnected".to_string())?
      .send(cmd)
      .map_err(|_| "network process disconnected".to_string())
  }

  pub fn recv_timeout(
    &self,
    timeout: Duration,
  ) -> Result<NetworkToRenderer, mpsc::RecvTimeoutError> {
    self.event_rx.recv_timeout(timeout)
  }

  /// Shut down the network process and join its thread.
  pub fn shutdown(mut self) {
    drop(self.cmd_tx.take());
    if let Some(join) = self.join.take() {
      let _ = join.join();
    }
  }
}

impl Drop for WebSocketNetworkProcessHandle {
  fn drop(&mut self) {
    if std::thread::panicking() {
      return;
    }
    drop(self.cmd_tx.take());
    if let Some(join) = self.join.take() {
      let _ = join.join();
    }
  }
}

pub struct WebSocketNetworkProcess;

impl WebSocketNetworkProcess {
  /// Spawn an in-process network-thread that services WebSocket IPC commands.
  pub fn spawn() -> WebSocketNetworkProcessHandle {
    let (cmd_tx, cmd_rx) = mpsc::sync_channel::<RendererToNetwork>(4096);
    let (event_tx, event_rx) = mpsc::sync_channel::<NetworkToRenderer>(4096);
    let join = std::thread::Builder::new()
      .name("fastrender-network-websocket".to_string())
      .spawn(move || network_main(cmd_rx, event_tx))
      .expect("spawn network websocket thread");

    WebSocketNetworkProcessHandle {
      cmd_tx: Some(cmd_tx),
      event_rx,
      join: Some(join),
    }
  }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WebSocketRuntimeMetrics {
  pub active_connections: usize,
  pub pending_event_bytes: usize,
}

static ACTIVE_CONNECTIONS: AtomicUsize = AtomicUsize::new(0);
static TOTAL_PENDING_EVENT_BYTES: AtomicUsize = AtomicUsize::new(0);

pub fn websocket_runtime_metrics() -> WebSocketRuntimeMetrics {
  WebSocketRuntimeMetrics {
    active_connections: ACTIVE_CONNECTIONS.load(Ordering::Relaxed),
    pending_event_bytes: TOTAL_PENDING_EVENT_BYTES.load(Ordering::Relaxed),
  }
}

fn websocket_io_runtime() -> &'static tokio::runtime::Runtime {
  static RT: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
  RT.get_or_init(|| {
    tokio::runtime::Builder::new_multi_thread()
      .enable_all()
      .worker_threads(WS_IO_WORKER_THREADS)
      .max_blocking_threads(WS_IO_MAX_BLOCKING_THREADS)
      .thread_name_fn(|| {
        static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(1);
        let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        // Linux thread names are limited to 15 bytes; keep this prefix short so tests can
        // reliably identify the runtime threads via `/proc/self/task/*/comm`.
        format!("fr-wsio-{n}")
      })
      .build()
      .expect("build websocket tokio runtime")
  })
}

fn network_main(cmd_rx: mpsc::Receiver<RendererToNetwork>, event_tx: mpsc::SyncSender<NetworkToRenderer>) {
  let (ws_msg_tx, ws_msg_rx) = mpsc::sync_channel::<WsThreadMsg>(4096);
  let mut sockets: HashMap<u64, SocketEntry> = HashMap::new();

  // Initialize the Tokio runtime upfront so its thread creation happens before we accept renderer
  // commands (best-effort; avoids surprising latency spikes on first connect).
  let _ = websocket_io_runtime();

  let mut shutdown_all = |sockets: &mut HashMap<u64, SocketEntry>| {
    for (_conn_id, entry) in sockets.iter_mut() {
      if let Some(cmd_tx) = entry.cmd_tx.as_ref() {
        let _ = cmd_tx.try_send(WsCommand::Shutdown);
      }
      if let Some(task) = entry.task.as_ref() {
        task.abort();
      }
    }
    for (_conn_id, mut entry) in sockets.drain() {
      TOTAL_PENDING_EVENT_BYTES.fetch_sub(entry.pending_event_bytes, Ordering::Relaxed);
      entry.pending_event_bytes = 0;
      drop(entry.cmd_tx.take());
      if let Some(task) = entry.task.take() {
        task.abort();
      }
    }
  };

  let mut running = true;
  while running {
    loop {
      match cmd_rx.try_recv() {
        Ok(cmd) => handle_network_command(cmd, &mut sockets, &ws_msg_tx),
        Err(mpsc::TryRecvError::Empty) => break,
        Err(mpsc::TryRecvError::Disconnected) => {
          running = false;
          break;
        }
      }
    }

      loop {
        match ws_msg_rx.try_recv() {
          Ok(msg) => match msg {
            WsThreadMsg::Event {
              conn_id,
              generation,
              event,
            } => enqueue_ws_event(conn_id, generation, event, &mut sockets),
          },
          Err(mpsc::TryRecvError::Empty) => break,
          Err(mpsc::TryRecvError::Disconnected) => break,
        }
      }

    if flush_events(&event_tx, &mut sockets).is_err() {
      // Renderer disconnected.
      running = false;
    }

    if running {
      std::thread::sleep(Duration::from_millis(1));
    }
  }

  shutdown_all(&mut sockets);
}

fn handle_network_command(
  msg: RendererToNetwork,
  sockets: &mut HashMap<u64, SocketEntry>,
  ws_msg_tx: &mpsc::SyncSender<WsThreadMsg>,
) {
  match msg {
    RendererToNetwork::WebSocket { conn_id, cmd } => handle_websocket_command(conn_id, cmd, sockets, ws_msg_tx),
  }
}

fn handle_websocket_command(
  conn_id: u64,
  cmd: WebSocketCommand,
  sockets: &mut HashMap<u64, SocketEntry>,
  ws_msg_tx: &mpsc::SyncSender<WsThreadMsg>,
) {
  if let Err(err) = cmd.validate() {
    // Deterministic rejection: report error + close without spawning a worker.
    queue_local_rejection(conn_id, err, sockets);
    return;
  }

  match cmd {
    WebSocketCommand::Connect { params } => {
      if let Err(err) = params.validate() {
        queue_local_rejection(conn_id, err, sockets);
        return;
      }

      // Replace any existing connection with the same id. Track a monotonically increasing
      // generation so late events from an old task cannot be misdelivered to a new connection.
      let generation = if let Some(mut existing) = sockets.remove(&conn_id) {
        if let Some(cmd_tx) = existing.cmd_tx.as_ref() {
          let _ = cmd_tx.try_send(WsCommand::Shutdown);
        }
        if let Some(task) = existing.task.take() {
          task.abort();
        }
        TOTAL_PENDING_EVENT_BYTES.fetch_sub(existing.pending_event_bytes, Ordering::Relaxed);
        existing.generation.saturating_add(1)
      } else {
        1
      };

      let (cmd_tx, cmd_rx) = tokio_mpsc::channel::<WsCommand>(MAX_QUEUED_WEBSOCKET_SEND_COMMANDS);
      let ws_msg_tx = ws_msg_tx.clone();
      let task = websocket_io_runtime().spawn(ws_worker_task(conn_id, generation, params, cmd_rx, ws_msg_tx));

      sockets.insert(
        conn_id,
        SocketEntry {
          generation,
          cmd_tx: Some(cmd_tx),
          task: Some(task),
          pending_events: VecDeque::new(),
          pending_event_bytes: 0,
          overflowed: false,
        },
      );
    }
    WebSocketCommand::SendText { text } => {
      if let Some(entry) = sockets.get_mut(&conn_id) {
        if entry.overflowed {
          return;
        }
        if let Some(cmd_tx) = entry.cmd_tx.as_ref() {
          match cmd_tx.try_send(WsCommand::SendText(text)) {
            Ok(()) => {}
            Err(tokio_mpsc::error::TrySendError::Full(_)) => {
              // Treat as misbehaving renderer; tear down to keep resource use bounded.
              overflow_socket(
                conn_id,
                entry,
                CLOSE_CODE_POLICY_VIOLATION,
                "websocket send queue is full".to_string(),
              );
            }
            Err(tokio_mpsc::error::TrySendError::Closed(_)) => {
              entry.cmd_tx = None;
            }
          }
        }
      }
    }
    WebSocketCommand::SendBinary { data } => {
      if let Some(entry) = sockets.get_mut(&conn_id) {
        if entry.overflowed {
          return;
        }
        if let Some(cmd_tx) = entry.cmd_tx.as_ref() {
          match cmd_tx.try_send(WsCommand::SendBinary(data)) {
            Ok(()) => {}
            Err(tokio_mpsc::error::TrySendError::Full(_)) => {
              overflow_socket(
                conn_id,
                entry,
                CLOSE_CODE_POLICY_VIOLATION,
                "websocket send queue is full".to_string(),
              );
            }
            Err(tokio_mpsc::error::TrySendError::Closed(_)) => {
              entry.cmd_tx = None;
            }
          }
        }
      }
    }
    WebSocketCommand::Close { code, reason } => {
      if let Some(entry) = sockets.get_mut(&conn_id) {
        if let Some(cmd_tx) = entry.cmd_tx.as_ref() {
          let _ = cmd_tx.try_send(WsCommand::Close { code, reason });
        }
      }
    }
    WebSocketCommand::Shutdown => {
      if let Some(entry) = sockets.get_mut(&conn_id) {
        if let Some(cmd_tx) = entry.cmd_tx.as_ref() {
          let _ = cmd_tx.try_send(WsCommand::Shutdown);
        }
      }
    }
  }
}

fn queue_local_rejection(conn_id: u64, err: WebSocketValidationError, sockets: &mut HashMap<u64, SocketEntry>) {
  let entry = sockets.entry(conn_id).or_insert_with(|| SocketEntry {
    generation: 1,
    cmd_tx: None,
    task: None,
    pending_events: VecDeque::new(),
    pending_event_bytes: 0,
    overflowed: true,
  });

  // Don't let repeated invalid commands build up a queue.
  if entry.pending_events.len() >= MAX_QUEUED_WEBSOCKET_EVENTS_PER_SOCKET {
    TOTAL_PENDING_EVENT_BYTES.fetch_sub(entry.pending_event_bytes, Ordering::Relaxed);
    entry.pending_events.clear();
    entry.pending_event_bytes = 0;
  }

  entry.pending_events.push_back(WebSocketEvent::Error {
    message: Some(err.to_string()),
  });
  entry.pending_events.push_back(WebSocketEvent::Close {
    code: CLOSE_CODE_POLICY_VIOLATION,
    reason: "invalid websocket command".to_string(),
  });
}

fn event_payload_bytes(ev: &WebSocketEvent) -> usize {
  match ev {
    WebSocketEvent::MessageText { text } => text.as_bytes().len(),
    WebSocketEvent::MessageBinary { data } => data.len(),
    _ => 0,
  }
}

fn enqueue_ws_event(
  conn_id: u64,
  generation: u64,
  ev: WebSocketEvent,
  sockets: &mut HashMap<u64, SocketEntry>,
) {
  let Some(entry) = sockets.get_mut(&conn_id) else {
    return;
  };
  if entry.generation != generation {
    return;
  }
  if entry.overflowed {
    return;
  }

  let payload_bytes = event_payload_bytes(&ev);
  let next_bytes = entry.pending_event_bytes.saturating_add(payload_bytes);
  if next_bytes > MAX_WEBSOCKET_PENDING_EVENT_BYTES {
    overflow_socket(
      conn_id,
      entry,
      CLOSE_CODE_MESSAGE_TOO_BIG,
      "event queue overflow".to_string(),
    );
    return;
  }

  if entry.pending_events.len() >= MAX_QUEUED_WEBSOCKET_EVENTS_PER_SOCKET {
    overflow_socket(
      conn_id,
      entry,
      CLOSE_CODE_MESSAGE_TOO_BIG,
      "event queue overflow".to_string(),
    );
    return;
  }

  entry.pending_event_bytes = next_bytes;
  TOTAL_PENDING_EVENT_BYTES.fetch_add(payload_bytes, Ordering::Relaxed);
  entry.pending_events.push_back(ev);
}

fn overflow_socket(conn_id: u64, entry: &mut SocketEntry, close_code: u16, reason: String) {
  entry.overflowed = true;
  if let Some(cmd_tx) = entry.cmd_tx.as_ref() {
    let _ = cmd_tx.try_send(WsCommand::Shutdown);
  }
  // Drop the sender so the worker sees disconnection even if its queue is full.
  drop(entry.cmd_tx.take());
  if let Some(task) = entry.task.take() {
    task.abort();
  }

  TOTAL_PENDING_EVENT_BYTES.fetch_sub(entry.pending_event_bytes, Ordering::Relaxed);
  entry.pending_events.clear();
  entry.pending_event_bytes = 0;
  entry.pending_events.push_back(WebSocketEvent::Error {
    message: Some(reason.clone()),
  });
  entry.pending_events.push_back(WebSocketEvent::Close {
    code: close_code,
    reason,
  });

  // Ensure we keep an entry around so the close can be flushed even though the worker is gone.
  let _ = conn_id;
}

fn flush_events(
  event_tx: &mpsc::SyncSender<NetworkToRenderer>,
  sockets: &mut HashMap<u64, SocketEntry>,
) -> Result<(), ()> {
  loop {
    let mut made_progress = false;
    let mut closed: Vec<u64> = Vec::new();

    let keys: Vec<u64> = sockets.keys().copied().collect();
    for conn_id in keys {
      let Some(entry) = sockets.get_mut(&conn_id) else {
        continue;
      };
      let Some(ev) = entry.pending_events.pop_front() else {
        continue;
      };
      let payload_bytes = event_payload_bytes(&ev);
      entry.pending_event_bytes = entry.pending_event_bytes.saturating_sub(payload_bytes);
      TOTAL_PENDING_EVENT_BYTES.fetch_sub(payload_bytes, Ordering::Relaxed);
      let is_close = matches!(ev, WebSocketEvent::Close { .. });
      match event_tx.try_send(NetworkToRenderer::WebSocket { conn_id, event: ev }) {
        Ok(()) => {
          made_progress = true;
          if is_close {
            closed.push(conn_id);
          }
        }
        Err(mpsc::TrySendError::Full(NetworkToRenderer::WebSocket { conn_id: _, event })) => {
          // Renderer is not draining the IPC channel: treat as backpressure and close this socket to
          // keep memory bounded.
          entry.pending_events.push_front(event);
          entry.pending_event_bytes = entry.pending_event_bytes.saturating_add(payload_bytes);
          TOTAL_PENDING_EVENT_BYTES.fetch_add(payload_bytes, Ordering::Relaxed);
          overflow_socket(
            conn_id,
            entry,
            CLOSE_CODE_MESSAGE_TOO_BIG,
            "renderer websocket event channel full".to_string(),
          );
          return Ok(());
        }
        Err(mpsc::TrySendError::Disconnected(_)) => return Err(()),
      }
    }

    for conn_id in closed {
      if let Some(mut entry) = sockets.remove(&conn_id) {
        TOTAL_PENDING_EVENT_BYTES.fetch_sub(entry.pending_event_bytes, Ordering::Relaxed);
        drop(entry.cmd_tx.take());
        if let Some(task) = entry.task.take() {
          task.abort();
        }
      }
    }

    if !made_progress {
      break;
    }
  }

  Ok(())
}

async fn ws_worker_task(
  conn_id: u64,
  generation: u64,
  params: WebSocketConnectParams,
  mut cmd_rx: tokio_mpsc::Receiver<WsCommand>,
  ws_msg_tx: mpsc::SyncSender<WsThreadMsg>,
) {
  ACTIVE_CONNECTIONS.fetch_add(1, Ordering::Relaxed);
  struct ActiveGuard;
  impl Drop for ActiveGuard {
    fn drop(&mut self) {
      ACTIVE_CONNECTIONS.fetch_sub(1, Ordering::Relaxed);
    }
  }
  let _active_guard = ActiveGuard;

  let connect_res = timeout(WS_CONNECT_TIMEOUT, connect_socket(&params)).await;
  let (mut socket, selected_protocol) = match connect_res {
    Ok(Ok(v)) => v,
    Ok(Err(err)) => {
      let _ = ws_msg_tx.try_send(WsThreadMsg::Event {
        conn_id,
        generation,
        event: WebSocketEvent::Error {
          message: Some(err),
        },
      });
      let _ = ws_msg_tx.try_send(WsThreadMsg::Event {
        conn_id,
        generation,
        event: WebSocketEvent::Close {
          code: CLOSE_CODE_POLICY_VIOLATION,
          reason: "connect failed".to_string(),
        },
      });
      return;
    }
    Err(_elapsed) => {
      let _ = ws_msg_tx.try_send(WsThreadMsg::Event {
        conn_id,
        generation,
        event: WebSocketEvent::Error {
          message: Some("connect timed out".to_string()),
        },
      });
      let _ = ws_msg_tx.try_send(WsThreadMsg::Event {
        conn_id,
        generation,
        event: WebSocketEvent::Close {
          code: CLOSE_CODE_POLICY_VIOLATION,
          reason: "connect failed".to_string(),
        },
      });
      return;
    }
  };

  let _ = ws_msg_tx.try_send(WsThreadMsg::Event {
    conn_id,
    generation,
    event: WebSocketEvent::Open {
      selected_protocol: selected_protocol.clone(),
    },
  });

  let mut close_info: Option<(u16, String)> = None;

  while close_info.is_none() {
    tokio::select! {
      biased;
      cmd = cmd_rx.recv() => {
        match cmd {
          Some(WsCommand::SendText(text)) => {
            let len = text.as_bytes().len();
            match timeout(WS_WRITE_TIMEOUT, socket.send(Message::Text(text))).await {
              Ok(Ok(())) => {
                let _ = ws_msg_tx.try_send(WsThreadMsg::Event {
                  conn_id,
                  generation,
                  event: WebSocketEvent::SendAck { bytes: len as u32 },
                });
              }
              _ => {
                close_info = Some((CLOSE_CODE_ABNORMAL, "".to_string()));
              }
            }
          }
          Some(WsCommand::SendBinary(data)) => {
            let len = data.len();
            match timeout(WS_WRITE_TIMEOUT, socket.send(Message::Binary(data))).await {
              Ok(Ok(())) => {
                let _ = ws_msg_tx.try_send(WsThreadMsg::Event {
                  conn_id,
                  generation,
                  event: WebSocketEvent::SendAck { bytes: len as u32 },
                });
              }
              _ => {
                close_info = Some((CLOSE_CODE_ABNORMAL, "".to_string()));
              }
            }
          }
          Some(WsCommand::Close { code, reason }) => {
            let code = WebSocketCommand::normalized_close_code(code);
            let reason = reason.unwrap_or_default();
            close_info = Some((code, reason.clone()));
            let frame = CloseFrame {
              code: tungstenite::protocol::frame::coding::CloseCode::from(code),
              reason: Cow::Owned(reason),
            };
            let _ = timeout(WS_WRITE_TIMEOUT, socket.send(Message::Close(Some(frame)))).await;
          }
          Some(WsCommand::Shutdown) | None => {
            close_info = Some((CLOSE_CODE_GOING_AWAY, "shutdown".to_string()));
            let _ = timeout(WS_WRITE_TIMEOUT, socket.send(Message::Close(None))).await;
          }
        }
      }
      msg = socket.next() => {
        match msg {
          Some(Ok(Message::Text(text))) => {
            if text.as_bytes().len() > MAX_WEBSOCKET_MESSAGE_BYTES as usize {
              close_info = Some((CLOSE_CODE_MESSAGE_TOO_BIG, "message too large".to_string()));
              let frame = CloseFrame {
                code: tungstenite::protocol::frame::coding::CloseCode::from(CLOSE_CODE_MESSAGE_TOO_BIG),
                reason: Cow::Borrowed("message too large"),
              };
              let _ = timeout(WS_WRITE_TIMEOUT, socket.send(Message::Close(Some(frame)))).await;
              break;
            }
            let _ = ws_msg_tx.try_send(WsThreadMsg::Event {
              conn_id,
              generation,
              event: WebSocketEvent::MessageText { text },
            });
          }
          Some(Ok(Message::Binary(data))) => {
            if data.len() > MAX_WEBSOCKET_MESSAGE_BYTES as usize {
              close_info = Some((CLOSE_CODE_MESSAGE_TOO_BIG, "message too large".to_string()));
              let frame = CloseFrame {
                code: tungstenite::protocol::frame::coding::CloseCode::from(CLOSE_CODE_MESSAGE_TOO_BIG),
                reason: Cow::Borrowed("message too large"),
              };
              let _ = timeout(WS_WRITE_TIMEOUT, socket.send(Message::Close(Some(frame)))).await;
              break;
            }
            let _ = ws_msg_tx.try_send(WsThreadMsg::Event {
              conn_id,
              generation,
              event: WebSocketEvent::MessageBinary { data },
            });
          }
          Some(Ok(Message::Close(frame))) => {
            let (code, reason) = frame
              .as_ref()
              .map(|f| (u16::from(f.code), f.reason.to_string()))
              .unwrap_or((1000, "".to_string()));
            close_info = Some((code, clamp_close_reason(reason)));
            let _ = timeout(WS_WRITE_TIMEOUT, socket.send(Message::Close(frame))).await;
            break;
          }
          Some(Ok(Message::Ping(payload))) => {
            if timeout(WS_WRITE_TIMEOUT, socket.send(Message::Pong(payload))).await.is_err() {
              close_info = Some((CLOSE_CODE_ABNORMAL, "".to_string()));
              break;
            }
          }
          Some(Ok(Message::Pong(_))) => {}
          Some(Err(err)) => {
            let _ = ws_msg_tx.try_send(WsThreadMsg::Event {
              conn_id,
              generation,
              event: WebSocketEvent::Error {
                message: Some(err.to_string()),
              },
            });
            close_info = Some((CLOSE_CODE_ABNORMAL, "".to_string()));
            break;
          }
          None => {
            close_info = Some((CLOSE_CODE_GOING_AWAY, "shutdown".to_string()));
            break;
          }
          _ => {}
        }
      }
    }
  }

  let _ = timeout(WS_SHUTDOWN_TIMEOUT, socket.close()).await;

  let closing = close_info.unwrap_or((1000, "".to_string()));
  let _ = ws_msg_tx.try_send(WsThreadMsg::Event {
    conn_id,
    generation,
    event: WebSocketEvent::Close {
      code: closing.0,
      reason: clamp_close_reason(closing.1),
    },
  });
}

fn clamp_close_reason(mut reason: String) -> String {
  if reason.as_bytes().len() <= MAX_WEBSOCKET_CLOSE_REASON_BYTES as usize {
    return reason;
  }
  // Drop the reason entirely rather than trying to slice at UTF-8 boundaries; this is defensive
  // against malicious servers and keeps behavior deterministic.
  reason.clear();
  reason
}

async fn connect_socket(
  params: &WebSocketConnectParams,
) -> Result<
  (
    tokio_tungstenite::WebSocketStream<
      tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
    String,
  ),
  String,
> {
  // Re-parse and normalize the URL in the network process. The renderer is untrusted, and this also
  // ensures `http:`/`https:` forms are converted to `ws:`/`wss:` deterministically.
  let url = params.validated_url().map_err(|err| err.to_string())?;
  let mut req = url
    .as_str()
    .into_client_request()
    .map_err(|err| err.to_string())?;

  if !params.protocols.is_empty() {
    let joined = params.protocols.join(", ");
    req
      .headers_mut()
      .insert(
        "Sec-WebSocket-Protocol",
        http::HeaderValue::from_str(&joined).map_err(|e| e.to_string())?,
      );
  }

  if let Some(origin) = params.origin.as_deref() {
    req
      .headers_mut()
      .insert(
        "Origin",
        http::HeaderValue::from_str(origin).map_err(|e| e.to_string())?,
      );
  }

  let (socket, response) = tokio_tungstenite::connect_async(req)
    .await
    .map_err(|e| e.to_string())?;
  let selected_protocol = response
    .headers()
    .get("Sec-WebSocket-Protocol")
    .and_then(|h| h.to_str().ok())
    .unwrap_or("")
    .to_string();

  Ok((socket, selected_protocol))
}

#[cfg(all(test, feature = "direct_websocket"))]
mod tests {
  use super::*;
  use crate::testing::{net_test_lock, try_bind_localhost};
  use std::time::Instant;

  #[cfg(target_os = "linux")]
  fn count_threads_with_name_prefix(prefix: &str) -> usize {
    let mut count = 0usize;
    let Ok(entries) = std::fs::read_dir("/proc/self/task") else {
      return 0;
    };
    for entry in entries.flatten() {
      let name_path = entry.path().join("comm");
      if let Ok(name) = std::fs::read_to_string(name_path) {
        if name.trim().starts_with(prefix) {
          count = count.saturating_add(1);
        }
      }
    }
    count
  }

  #[cfg(not(target_os = "linux"))]
  fn count_threads_with_name_prefix(_prefix: &str) -> usize {
    0
  }

  #[test]
  fn websocket_echo_via_network_process() {
    let _net_guard = net_test_lock();
    let Some(listener) = try_bind_localhost("websocket_echo_via_network_process") else {
      return;
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
            let msg = loop {
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

    let net = WebSocketNetworkProcess::spawn();
    let conn_id = 1u64;

    net
      .send(RendererToNetwork::WebSocket {
        conn_id,
        cmd: WebSocketCommand::Connect {
          params: WebSocketConnectParams {
            url: format!("ws://{addr}/"),
            protocols: Vec::new(),
            origin: None,
            document_url: None,
          },
        },
      })
      .unwrap();

    let mut opened = false;
    let mut got_echo = false;
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline && !(opened && got_echo) {
      match net.recv_timeout(Duration::from_millis(50)) {
        Ok(NetworkToRenderer::WebSocket { conn_id: id, event }) => {
          assert_eq!(id, conn_id);
          match event {
            WebSocketEvent::Open { .. } => {
              opened = true;
              net
                .send(RendererToNetwork::WebSocket {
                  conn_id,
                  cmd: WebSocketCommand::SendText {
                    text: "hello".to_string(),
                  },
                })
                .unwrap();
            }
            WebSocketEvent::MessageText { text } => {
              assert_eq!(text, "hello");
              got_echo = true;
            }
            WebSocketEvent::SendAck { .. } => {}
            WebSocketEvent::Error { message } => panic!("unexpected websocket error: {message:?}"),
            WebSocketEvent::Close { .. } => {}
            other => panic!("unexpected event: {other:?}"),
          }
        }
        Err(mpsc::RecvTimeoutError::Timeout) => {}
        Err(err) => panic!("network recv error: {err:?}"),
      }
    }

    assert!(opened, "timed out waiting for Open event");
    assert!(got_echo, "timed out waiting for echo message");

    net
      .send(RendererToNetwork::WebSocket {
        conn_id,
        cmd: WebSocketCommand::Close {
          code: Some(1000),
          reason: None,
        },
      })
      .unwrap();

    let mut saw_close = false;
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline && !saw_close {
      match net.recv_timeout(Duration::from_millis(50)) {
        Ok(NetworkToRenderer::WebSocket { conn_id: id, event }) => {
          assert_eq!(id, conn_id);
          match event {
            WebSocketEvent::Close { .. } => saw_close = true,
            WebSocketEvent::Error { message } => panic!("unexpected websocket error: {message:?}"),
            _ => {}
          }
        }
        Err(mpsc::RecvTimeoutError::Timeout) => {}
        Err(err) => panic!("network recv error: {err:?}"),
      }
    }

    assert!(saw_close, "timed out waiting for Close event");

    net.shutdown();
    server.join().expect("server thread panicked");
  }

  #[cfg(target_os = "linux")]
  #[test]
  fn websocket_network_process_is_thread_bounded_under_many_connections() {
    let _net_guard = net_test_lock();
    let Some(listener) =
      try_bind_localhost("websocket_network_process_is_thread_bounded_under_many_connections")
    else {
      return;
    };
    listener.set_nonblocking(true).expect("set_nonblocking");
    let addr = listener.local_addr().expect("local_addr");

    const TOTAL_CONNS: usize = 500;

    let (ready_tx, ready_rx) = mpsc::channel::<()>();
    let server = std::thread::spawn(move || {
      let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("build tokio server runtime");

      rt.block_on(async move {
        use futures_util::{SinkExt as _, StreamExt as _};

        let listener = tokio::net::TcpListener::from_std(listener).expect("from_std");
        let _ = ready_tx.send(());

        let mut join_set = tokio::task::JoinSet::new();
        for _ in 0..TOTAL_CONNS {
          let (stream, _) = tokio::time::timeout(Duration::from_secs(20), listener.accept())
            .await
            .expect("accept timed out")
            .expect("accept");
          join_set.spawn(async move {
            let mut ws = tokio_tungstenite::accept_async(stream).await.expect("accept websocket");
            let msg = match tokio::time::timeout(Duration::from_secs(10), ws.next()).await {
              Ok(Some(Ok(msg))) => Some(msg),
              Ok(Some(Err(err))) => panic!("server read failed: {err}"),
              Ok(None) => None,
              Err(_) => None,
            };
            if let Some(msg) = msg {
              ws.send(msg).await.expect("echo");
            }
            // Best-effort close handshake; avoid waiting on peer.
            let _ = ws.send(Message::Close(None)).await;
          });
        }

        while let Some(res) = join_set.join_next().await {
          res.expect("server connection task");
        }
      });
    });
    ready_rx
      .recv_timeout(Duration::from_secs(5))
      .expect("server ready");

    // Ensure the bounded runtime threads are running before we start counting.
    let _ = websocket_io_runtime();

    let ws_threads_before = count_threads_with_name_prefix("fr-wsio-");
    assert!(
      ws_threads_before > 0,
      "expected websocket I/O runtime threads to be running"
    );
    assert!(
      ws_threads_before <= WS_IO_WORKER_THREADS + WS_IO_MAX_BLOCKING_THREADS,
      "unexpected websocket runtime thread count: {ws_threads_before}"
    );

    let metrics_before = websocket_runtime_metrics();

    let net = WebSocketNetworkProcess::spawn();
    for conn_id in 1..=TOTAL_CONNS as u64 {
      net
        .send(RendererToNetwork::WebSocket {
          conn_id,
          cmd: WebSocketCommand::Connect {
            params: WebSocketConnectParams {
              url: format!("ws://{addr}/"),
              protocols: Vec::new(),
              origin: None,
              document_url: None,
            },
          },
        })
        .unwrap();
    }

    let mut expected = vec![String::new(); TOTAL_CONNS + 1];
    for id in 1..=TOTAL_CONNS {
      expected[id] = format!("msg-{id}");
    }

    let mut opened = vec![false; TOTAL_CONNS + 1];
    let mut echoed = vec![false; TOTAL_CONNS + 1];
    let mut remaining = TOTAL_CONNS;

    let deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < deadline && remaining > 0 {
      match net.recv_timeout(Duration::from_millis(50)) {
        Ok(NetworkToRenderer::WebSocket { conn_id, event }) => {
          let idx = conn_id as usize;
          if idx == 0 || idx > TOTAL_CONNS {
            continue;
          }
          match event {
            WebSocketEvent::Open { .. } => {
              opened[idx] = true;
              net
                .send(RendererToNetwork::WebSocket {
                  conn_id,
                  cmd: WebSocketCommand::SendText {
                    text: expected[idx].clone(),
                  },
                })
                .unwrap();
            }
            WebSocketEvent::MessageText { text } => {
              assert!(
                opened[idx],
                "received message before Open for conn_id={conn_id}"
              );
              assert_eq!(text, expected[idx], "echo mismatch for conn_id={conn_id}");
              if !echoed[idx] {
                echoed[idx] = true;
                remaining = remaining.saturating_sub(1);
              }
              let _ = net.send(RendererToNetwork::WebSocket {
                conn_id,
                cmd: WebSocketCommand::Close {
                  code: Some(1000),
                  reason: None,
                },
              });
            }
            WebSocketEvent::SendAck { .. } => {}
            WebSocketEvent::Close { .. } => {}
            WebSocketEvent::Error { message } => {
              panic!("unexpected websocket error for {conn_id}: {message:?}")
            }
            other => panic!("unexpected event for {conn_id}: {other:?}"),
          }
        }
        Err(mpsc::RecvTimeoutError::Timeout) => {}
        Err(err) => panic!("network recv error: {err:?}"),
      }
    }

    assert_eq!(
      remaining, 0,
      "timed out waiting for {remaining} websocket echoes"
    );

    let ws_threads_after = count_threads_with_name_prefix("fr-wsio-");
    assert!(
      ws_threads_after <= WS_IO_WORKER_THREADS + WS_IO_MAX_BLOCKING_THREADS,
      "websocket runtime spawned too many threads (before={ws_threads_before}, after={ws_threads_after})"
    );

    net.shutdown();
    server.join().expect("server thread panicked");

    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
      let metrics = websocket_runtime_metrics();
      if metrics.active_connections <= metrics_before.active_connections
        && metrics.pending_event_bytes <= metrics_before.pending_event_bytes
      {
        break;
      }
      std::thread::sleep(Duration::from_millis(10));
    }

    let metrics_after = websocket_runtime_metrics();
    assert!(
      metrics_after.active_connections <= metrics_before.active_connections,
      "expected active_connections to return to baseline: before={metrics_before:?}, after={metrics_after:?}"
    );
    assert!(
      metrics_after.pending_event_bytes <= metrics_before.pending_event_bytes,
      "expected pending_event_bytes to return to baseline: before={metrics_before:?}, after={metrics_after:?}"
    );
  }

  #[test]
  fn websocket_network_process_closes_when_pending_event_bytes_overflow() {
    let _net_guard = net_test_lock();
    let Some(listener) =
      try_bind_localhost("websocket_network_process_closes_when_pending_event_bytes_overflow")
    else {
      return;
    };
    listener.set_nonblocking(true).expect("set_nonblocking");
    let addr = listener.local_addr().expect("local_addr");

    // Ensure we exceed the pending-byte cap without requiring huge allocations.
    let msg_len = (MAX_WEBSOCKET_PENDING_EVENT_BYTES / 4).saturating_add(1);
    assert!(
      msg_len <= MAX_WEBSOCKET_MESSAGE_BYTES as usize,
      "test message length must be within MAX_WEBSOCKET_MESSAGE_BYTES"
    );

    let server = std::thread::spawn(move || {
      let deadline = Instant::now() + Duration::from_secs(5);
      loop {
        match listener.accept() {
          Ok((stream, _)) => {
            let mut stream = stream;
            let _ = stream.set_read_timeout(Some(Duration::from_millis(50)));
            let _ = stream.set_write_timeout(Some(Duration::from_secs(5)));
            let mut ws = tungstenite::accept(stream).expect("accept websocket");

            // Flood the client quickly. The network process runtime should cap queued inbound
            // payload bytes and close the socket rather than accumulating unbounded memory.
            let payload = vec![0u8; msg_len];
            for _ in 0..64 {
              if ws
                .write_message(Message::Binary(payload.clone()))
                .is_err()
              {
                break;
              }
            }

            // Wait for the client to close.
            let close_deadline = Instant::now() + Duration::from_secs(5);
            loop {
              match ws.read_message() {
                Ok(Message::Close(_)) => break,
                Ok(_) => {}
                Err(tungstenite::Error::ConnectionClosed)
                | Err(tungstenite::Error::AlreadyClosed)
                | Err(tungstenite::Error::Protocol(_)) => break,
                Err(tungstenite::Error::Io(ref err))
                  if matches!(
                    err.kind(),
                    std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
                  ) =>
                {
                  if Instant::now() >= close_deadline {
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

    let net = WebSocketNetworkProcess::spawn();
    let conn_id = 1u64;
    net
      .send(RendererToNetwork::WebSocket {
        conn_id,
        cmd: WebSocketCommand::Connect {
          params: WebSocketConnectParams {
            url: format!("ws://{addr}/"),
            protocols: Vec::new(),
            origin: None,
            document_url: None,
          },
        },
      })
      .unwrap();

    let mut saw_close = false;
    let mut close_code = 0u16;
    let mut close_reason = String::new();
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline && !saw_close {
      match net.recv_timeout(Duration::from_millis(50)) {
        Ok(NetworkToRenderer::WebSocket { conn_id: id, event }) => {
          assert_eq!(id, conn_id);
          if let WebSocketEvent::Close { code, reason } = event {
            saw_close = true;
            close_code = code;
            close_reason = reason;
          }
        }
        Err(mpsc::RecvTimeoutError::Timeout) => {}
        Err(err) => panic!("network recv error: {err:?}"),
      }
    }

    assert!(saw_close, "timed out waiting for Close event");
    assert_eq!(
      close_code, CLOSE_CODE_MESSAGE_TOO_BIG,
      "expected close(1009) when the network process pending-byte cap is exceeded"
    );
    assert!(
      close_reason.contains("overflow"),
      "expected close reason to mention overflow, got {close_reason:?}"
    );

    net.shutdown();
    server.join().expect("server thread panicked");
  }
}
