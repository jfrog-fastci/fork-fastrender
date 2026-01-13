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
//! - One dedicated OS thread per WebSocket connection (mirrors `window_websocket.rs`).
//! - Each worker thread performs the connect handshake and handles socket I/O.
//! - All renderer-supplied commands are validated via [`crate::ipc::websocket::WebSocketCommand`].
//! - Network → renderer events are queued per-socket with a hard cap to prevent unbounded memory
//!   growth (backpressure). On overflow, the socket is closed and an error is sent.

use crate::ipc::network::{NetworkToRenderer, RendererToNetwork};
use crate::ipc::websocket::{
  WebSocketCommand, WebSocketConnectParams, WebSocketEvent, WebSocketValidationError,
  MAX_WEBSOCKET_CLOSE_REASON_BYTES, MAX_WEBSOCKET_MESSAGE_BYTES,
};
use std::borrow::Cow;
use std::collections::{HashMap, VecDeque};
use std::sync::mpsc;
use std::thread::JoinHandle;
use std::time::Duration;

use tungstenite::client::IntoClientRequest;
use tungstenite::protocol::{CloseFrame, Message};

const READ_TIMEOUT: Duration = Duration::from_millis(50);

/// Matches `window_websocket.rs`'s `MAX_QUEUED_WEBSOCKET_EVENTS_PER_SOCKET`.
const MAX_QUEUED_WEBSOCKET_EVENTS_PER_SOCKET: usize = 1024;
/// Matches `window_websocket.rs`'s `MAX_QUEUED_WEBSOCKET_SEND_COMMANDS`.
const MAX_QUEUED_WEBSOCKET_SEND_COMMANDS: usize = 1024;

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
  Event { conn_id: u64, event: WebSocketEvent },
}

struct SocketEntry {
  cmd_tx: Option<mpsc::SyncSender<WsCommand>>,
  join: Option<JoinHandle<()>>,
  pending_events: VecDeque<WebSocketEvent>,
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

fn network_main(cmd_rx: mpsc::Receiver<RendererToNetwork>, event_tx: mpsc::SyncSender<NetworkToRenderer>) {
  let (ws_msg_tx, ws_msg_rx) = mpsc::sync_channel::<WsThreadMsg>(4096);
  let mut sockets: HashMap<u64, SocketEntry> = HashMap::new();

  let mut shutdown_all = |sockets: &mut HashMap<u64, SocketEntry>| {
    for (_conn_id, entry) in sockets.iter_mut() {
      if let Some(cmd_tx) = entry.cmd_tx.as_ref() {
        let _ = cmd_tx.try_send(WsCommand::Shutdown);
      }
    }
    for (_conn_id, mut entry) in sockets.drain() {
      drop(entry.cmd_tx.take());
      if let Some(join) = entry.join.take() {
        let _ = join.join();
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
          WsThreadMsg::Event { conn_id, event } => enqueue_ws_event(conn_id, event, &mut sockets),
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

      // Replace any existing connection with the same id.
      if let Some(mut existing) = sockets.remove(&conn_id) {
        if let Some(cmd_tx) = existing.cmd_tx.as_ref() {
          let _ = cmd_tx.try_send(WsCommand::Shutdown);
        }
        drop(existing.cmd_tx.take());
        if let Some(join) = existing.join.take() {
          let _ = join.join();
        }
      }

      let (cmd_tx, cmd_rx) = mpsc::sync_channel::<WsCommand>(MAX_QUEUED_WEBSOCKET_SEND_COMMANDS);
      let ws_msg_tx = ws_msg_tx.clone();
      let join = std::thread::Builder::new()
        .name(format!("fastrender-ws-{conn_id}"))
        .spawn(move || ws_worker_thread(conn_id, params, cmd_rx, ws_msg_tx))
        .expect("spawn websocket worker thread");

      sockets.insert(
        conn_id,
        SocketEntry {
          cmd_tx: Some(cmd_tx),
          join: Some(join),
          pending_events: VecDeque::new(),
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
            Err(mpsc::TrySendError::Full(_)) => {
              // Treat as misbehaving renderer; tear down to keep resource use bounded.
              overflow_socket(
                conn_id,
                entry,
                "websocket send queue is full".to_string(),
              );
            }
            Err(mpsc::TrySendError::Disconnected(_)) => {
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
            Err(mpsc::TrySendError::Full(_)) => {
              overflow_socket(
                conn_id,
                entry,
                "websocket send queue is full".to_string(),
              );
            }
            Err(mpsc::TrySendError::Disconnected(_)) => {
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
    cmd_tx: None,
    join: None,
    pending_events: VecDeque::new(),
    overflowed: true,
  });

  // Don't let repeated invalid commands build up a queue.
  if entry.pending_events.len() >= MAX_QUEUED_WEBSOCKET_EVENTS_PER_SOCKET {
    entry.pending_events.clear();
  }

  entry.pending_events.push_back(WebSocketEvent::Error {
    message: Some(err.to_string()),
  });
  entry.pending_events.push_back(WebSocketEvent::Close {
    code: CLOSE_CODE_POLICY_VIOLATION,
    reason: "invalid websocket command".to_string(),
  });
}

fn enqueue_ws_event(conn_id: u64, ev: WebSocketEvent, sockets: &mut HashMap<u64, SocketEntry>) {
  let Some(entry) = sockets.get_mut(&conn_id) else {
    return;
  };
  if entry.overflowed {
    return;
  }
  if entry.pending_events.len() >= MAX_QUEUED_WEBSOCKET_EVENTS_PER_SOCKET {
    overflow_socket(
      conn_id,
      entry,
      "websocket event queue overflow".to_string(),
    );
    return;
  }
  entry.pending_events.push_back(ev);
}

fn overflow_socket(conn_id: u64, entry: &mut SocketEntry, reason: String) {
  entry.overflowed = true;
  if let Some(cmd_tx) = entry.cmd_tx.as_ref() {
    let _ = cmd_tx.try_send(WsCommand::Shutdown);
  }
  // Drop the sender so the worker sees disconnection even if its queue is full.
  drop(entry.cmd_tx.take());
  if let Some(join) = entry.join.take() {
    let _ = join.join();
  }

  entry.pending_events.clear();
  entry.pending_events.push_back(WebSocketEvent::Error {
    message: Some(reason.clone()),
  });
  entry.pending_events.push_back(WebSocketEvent::Close {
    code: CLOSE_CODE_POLICY_VIOLATION,
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
      let is_close = matches!(ev, WebSocketEvent::Close { .. });
      match event_tx.try_send(NetworkToRenderer::WebSocket { conn_id, event: ev }) {
        Ok(()) => {
          made_progress = true;
          if is_close {
            closed.push(conn_id);
          }
        }
        Err(mpsc::TrySendError::Full(NetworkToRenderer::WebSocket { conn_id: _, event })) => {
          // Put the event back and stop flushing until the renderer drains.
          entry.pending_events.push_front(event);
          return Ok(());
        }
        Err(mpsc::TrySendError::Disconnected(_)) => return Err(()),
      }
    }

    for conn_id in closed {
      if let Some(mut entry) = sockets.remove(&conn_id) {
        drop(entry.cmd_tx.take());
        if let Some(join) = entry.join.take() {
          let _ = join.join();
        }
      }
    }

    if !made_progress {
      break;
    }
  }

  Ok(())
}

fn ws_worker_thread(
  conn_id: u64,
  params: WebSocketConnectParams,
  cmd_rx: mpsc::Receiver<WsCommand>,
  ws_msg_tx: mpsc::SyncSender<WsThreadMsg>,
) {
  let closing: (u16, String);

  let connect_res = connect_socket(&params);
  let (mut socket, selected_protocol) = match connect_res {
    Ok(v) => v,
    Err(err) => {
      let _ = ws_msg_tx.try_send(WsThreadMsg::Event {
        conn_id,
        event: WebSocketEvent::Error {
          message: Some(err),
        },
      });
      let _ = ws_msg_tx.try_send(WsThreadMsg::Event {
        conn_id,
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
    event: WebSocketEvent::Open {
      selected_protocol: selected_protocol.clone(),
    },
  });

  set_read_timeout(&mut socket);

  let mut close_info: Option<(u16, String)> = None;

  loop {
    // Drain commands.
    loop {
      match cmd_rx.try_recv() {
        Ok(WsCommand::SendText(text)) => {
          let len = text.as_bytes().len();
          let write_res = socket.write_message(Message::Text(text));
          if write_res.is_err() {
            close_info = Some((CLOSE_CODE_ABNORMAL, "".to_string()));
            break;
          }
          let _ = ws_msg_tx.try_send(WsThreadMsg::Event {
            conn_id,
            event: WebSocketEvent::SendAck {
              bytes: len as u32,
            },
          });
        }
        Ok(WsCommand::SendBinary(data)) => {
          let len = data.len();
          let write_res = socket.write_message(Message::Binary(data));
          if write_res.is_err() {
            close_info = Some((CLOSE_CODE_ABNORMAL, "".to_string()));
            break;
          }
          let _ = ws_msg_tx.try_send(WsThreadMsg::Event {
            conn_id,
            event: WebSocketEvent::SendAck {
              bytes: len as u32,
            },
          });
        }
        Ok(WsCommand::Close { code, reason }) => {
          let code = WebSocketCommand::normalized_close_code(code);
          let reason = reason.unwrap_or_default();
          close_info = Some((code, reason.clone()));
          let frame = CloseFrame {
            code: tungstenite::protocol::frame::coding::CloseCode::from(code),
            reason: Cow::Owned(reason),
          };
          let _ = socket.close(Some(frame));
          break;
        }
        Ok(WsCommand::Shutdown) => {
          close_info = Some((CLOSE_CODE_GOING_AWAY, "shutdown".to_string()));
          let _ = socket.close(None);
          break;
        }
        Err(mpsc::TryRecvError::Empty) => break,
        Err(mpsc::TryRecvError::Disconnected) => {
          close_info = Some((CLOSE_CODE_GOING_AWAY, "shutdown".to_string()));
          let _ = socket.close(None);
          break;
        }
      }
    }

    if close_info.is_some() {
      break;
    }

    match socket.read_message() {
      Ok(Message::Text(text)) => {
        if text.as_bytes().len() > MAX_WEBSOCKET_MESSAGE_BYTES as usize {
          close_info = Some((CLOSE_CODE_MESSAGE_TOO_BIG, "message too large".to_string()));
          let frame = CloseFrame {
            code: tungstenite::protocol::frame::coding::CloseCode::from(CLOSE_CODE_MESSAGE_TOO_BIG),
            reason: Cow::Borrowed("message too large"),
          };
          let _ = socket.close(Some(frame));
          break;
        }
        let _ = ws_msg_tx.try_send(WsThreadMsg::Event {
          conn_id,
          event: WebSocketEvent::MessageText { text },
        });
      }
      Ok(Message::Binary(data)) => {
        if data.len() > MAX_WEBSOCKET_MESSAGE_BYTES as usize {
          close_info = Some((CLOSE_CODE_MESSAGE_TOO_BIG, "message too large".to_string()));
          let frame = CloseFrame {
            code: tungstenite::protocol::frame::coding::CloseCode::from(CLOSE_CODE_MESSAGE_TOO_BIG),
            reason: Cow::Borrowed("message too large"),
          };
          let _ = socket.close(Some(frame));
          break;
        }
        let _ = ws_msg_tx.try_send(WsThreadMsg::Event {
          conn_id,
          event: WebSocketEvent::MessageBinary { data },
        });
      }
      Ok(Message::Close(frame)) => {
        let (code, reason) = frame
          .as_ref()
          .map(|f| (u16::from(f.code), f.reason.to_string()))
          .unwrap_or((1000, "".to_string()));
        close_info = Some((code, clamp_close_reason(reason)));
        let _ = socket.close(frame);
        break;
      }
      Ok(Message::Ping(payload)) => {
        // RFC 6455: endpoints must respond to pings with pongs containing the same payload.
        if socket.write_message(Message::Pong(payload)).is_err() {
          close_info = Some((CLOSE_CODE_ABNORMAL, "".to_string()));
          break;
        }
      }
      Ok(Message::Pong(_)) => {}
      Err(tungstenite::Error::Io(ref io))
        if matches!(
          io.kind(),
          std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
        ) => {}
      Err(err) => {
        let _ = ws_msg_tx.try_send(WsThreadMsg::Event {
          conn_id,
          event: WebSocketEvent::Error {
            message: Some(err.to_string()),
          },
        });
        close_info = Some((CLOSE_CODE_ABNORMAL, "".to_string()));
        break;
      }
      _ => {}
    }
  }

  closing = close_info.unwrap_or((1000, "".to_string()));

  let _ = ws_msg_tx.try_send(WsThreadMsg::Event {
    conn_id,
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

fn connect_socket(
  params: &WebSocketConnectParams,
) -> Result<
  (
    tungstenite::WebSocket<tungstenite::stream::MaybeTlsStream<std::net::TcpStream>>,
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

  let (socket, response) = tungstenite::connect(req).map_err(|e| e.to_string())?;
  let selected_protocol = response
    .headers()
    .get("Sec-WebSocket-Protocol")
    .and_then(|h| h.to_str().ok())
    .unwrap_or("")
    .to_string();

  Ok((socket, selected_protocol))
}

fn set_read_timeout(
  socket: &mut tungstenite::WebSocket<tungstenite::stream::MaybeTlsStream<std::net::TcpStream>>,
) {
  match socket.get_ref() {
    tungstenite::stream::MaybeTlsStream::Plain(stream) => {
      let _ = stream.set_read_timeout(Some(READ_TIMEOUT));
    }
    tungstenite::stream::MaybeTlsStream::Rustls(stream) => {
      let _ = stream.get_ref().set_read_timeout(Some(READ_TIMEOUT));
    }
    #[allow(unreachable_patterns)]
    _ => {}
  }
}

#[cfg(all(test, feature = "direct_websocket"))]
mod tests {
  use super::*;
  use crate::testing::{net_test_lock, try_bind_localhost};
  use std::time::Instant;

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
}
