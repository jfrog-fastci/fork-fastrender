#![cfg(feature = "direct_websocket")]

//! WebSocket IPC integration test.

use fastrender::dom2;
use fastrender::ipc::websocket::{WebSocketCommand, WebSocketEvent};
use fastrender::js::{
  install_window_websocket_ipc_bindings_with_guard, RunLimits, WebSocketIpcCommand, WebSocketIpcEvent,
  WindowHost, WindowHostState, WindowWebSocketIpcEnv,
};
use fastrender::resource::{FetchedResource, ResourceFetcher};
use fastrender::{Error, Result};
use selectors::context::QuirksMode;
use std::net::TcpListener;
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};
use tungstenite::handshake::server::{Request, Response};
use vm_js::{PropertyKey, Value};

#[derive(Debug, Default)]
struct NoFetchResourceFetcher;

impl ResourceFetcher for NoFetchResourceFetcher {
  fn fetch(&self, url: &str) -> Result<FetchedResource> {
    Err(Error::Other(format!(
      "NoFetchResourceFetcher does not support fetch: {url}"
    )))
  }
}

fn make_host(dom: dom2::Document, document_url: impl Into<String>) -> Result<WindowHost> {
  WindowHost::new_with_fetcher(dom, document_url, Arc::new(NoFetchResourceFetcher))
}

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

fn pump_until_done(host: &mut WindowHost, deadline: Instant) -> Result<()> {
  loop {
    let _ = host.run_until_idle(RunLimits {
      max_tasks: 200,
      max_microtasks: 1_000,
      max_wall_time: Some(Duration::from_millis(50)),
    })?;

    if get_global_prop_utf8(host, "__done").as_deref() == Some("true") {
      break;
    }
    if Instant::now() >= deadline {
      break;
    }
    std::thread::sleep(Duration::from_millis(10));
  }
  Ok(())
}

fn validate_ws_subprotocol_handshake_response(
  requested_protocols: &[String],
  response_headers: &http::HeaderMap,
) -> std::result::Result<String, ()> {
  let mut values = response_headers.get_all("Sec-WebSocket-Protocol").iter();
  let Some(value) = values.next() else {
    return Ok(String::new());
  };

  if values.next().is_some() {
    return Err(());
  }

  let value = value.to_str().map_err(|_| ())?;
  if value.is_empty() {
    return Err(());
  }
  if value
    .bytes()
    .any(|b| b == b',' || b.is_ascii_whitespace())
  {
    return Err(());
  }

  if requested_protocols.is_empty() {
    return Err(());
  }
  if !requested_protocols.iter().any(|p| p == value) {
    return Err(());
  }

  Ok(value.to_string())
}

#[test]
fn websocket_ipc_connect_send_echo_close() -> Result<()> {
  let Ok(listener) = TcpListener::bind("127.0.0.1:0") else {
    // Some sandboxed CI environments may forbid binding sockets; skip in that case.
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
          // Make the test deterministic: if the client never completes the handshake or never sends a
          // message, we want a bounded failure instead of hanging forever.
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

  // IPC channels between the renderer (WebSocket bindings) and the fake in-process network process.
  let (cmd_tx, cmd_rx) = mpsc::sync_channel::<WebSocketIpcCommand>(16);
  let (event_tx, event_rx) = mpsc::channel::<WebSocketIpcEvent>();

  let network = std::thread::spawn(move || {
    use tungstenite::client::IntoClientRequest;
    use tungstenite::protocol::Message;

    let connect_deadline = Instant::now() + Duration::from_secs(5);
    let (ws_id, url, protocols) = loop {
      match cmd_rx.recv_timeout(Duration::from_millis(50)) {
        Ok(WebSocketIpcCommand::WebSocket { conn_id, cmd }) => match cmd {
          WebSocketCommand::Connect { params } => {
            assert!(
              params
                .document_url
                .as_deref()
                .unwrap_or_default()
                .starts_with("http://"),
              "expected http:// document to be treated as an insecure context"
            );
            break (conn_id, params.url, params.protocols);
          }
          other => panic!("unexpected command before connect: {other:?}"),
        },
        Err(mpsc::RecvTimeoutError::Timeout) => {
          if Instant::now() >= connect_deadline {
            panic!("network process timed out waiting for connect command");
          }
        }
        Err(mpsc::RecvTimeoutError::Disconnected) => return,
      }
    };

    let mut req = url.into_client_request().expect("into_client_request");
    if !protocols.is_empty() {
      let header = protocols.join(", ");
      req
        .headers_mut()
        .insert("Sec-WebSocket-Protocol", header.parse().unwrap());
    }

    let (mut socket, response) = tungstenite::connect(req).expect("connect");
    let selected_protocol = match validate_ws_subprotocol_handshake_response(&protocols, response.headers()) {
      Ok(protocol) => protocol,
      Err(()) => {
        // RFC6455: server-selected protocol must match one of the requested protocols.
        event_tx
          .send(WebSocketIpcEvent::WebSocket {
            conn_id: ws_id,
            event: WebSocketEvent::Error {
              message: Some("invalid websocket subprotocol".to_string()),
            },
          })
          .expect("send error event");
        return;
      }
    };
    event_tx
      .send(WebSocketIpcEvent::WebSocket {
        conn_id: ws_id,
        event: WebSocketEvent::Open {
          selected_protocol,
        },
      })
      .expect("send open event");

    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
      match cmd_rx.recv_timeout(Duration::from_millis(50)) {
        Ok(WebSocketIpcCommand::WebSocket { conn_id, cmd }) => {
          assert_eq!(conn_id, ws_id);
          let mut did_send_text = false;
          match cmd {
            WebSocketCommand::SendText { text } => {
              did_send_text = true;
              let len = text.as_bytes().len();
              socket.write_message(Message::Text(text)).expect("write");
              event_tx
                .send(WebSocketIpcEvent::WebSocket {
                  conn_id: ws_id,
                  event: WebSocketEvent::SendAck { bytes: len as u32 },
                })
                .expect("send ack");
            }
            WebSocketCommand::SendBinary { .. } => {
              panic!("binary send not used by this test");
            }
            WebSocketCommand::Close { code, reason } => {
              let _ = socket.close(None);
              event_tx
                .send(WebSocketIpcEvent::WebSocket {
                  conn_id: ws_id,
                  event: WebSocketEvent::Close {
                    code: code.unwrap_or(1000),
                    reason: reason.unwrap_or_default(),
                  },
                })
                .expect("send close event");
              break;
            }
            WebSocketCommand::Connect { .. } => panic!("unexpected second connect"),
            WebSocketCommand::Shutdown => break,
          }
          if !did_send_text {
            continue;
          }

          // Keep the read bounded so we don't hang the test if the server never responds.
          match socket.get_ref() {
            tungstenite::stream::MaybeTlsStream::Plain(stream) => {
              let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
            }
            tungstenite::stream::MaybeTlsStream::Rustls(stream) => {
              let _ = stream
                .get_ref()
                .set_read_timeout(Some(Duration::from_secs(5)));
            }
            #[allow(unreachable_patterns)]
            _ => {}
          }
          let read_deadline = Instant::now() + Duration::from_secs(5);
          let msg = loop {
            match socket.read_message() {
              Ok(msg) => break msg,
              Err(tungstenite::Error::Io(ref err))
                if matches!(
                  err.kind(),
                  std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
                ) =>
              {
                if Instant::now() >= read_deadline {
                  panic!("network process read timed out");
                }
              }
              Err(err) => panic!("network process read failed: {err}"),
            }
          };
          match msg {
            Message::Text(text) => {
              event_tx
                .send(WebSocketIpcEvent::WebSocket {
                  conn_id: ws_id,
                  event: WebSocketEvent::MessageText { text },
                })
                .expect("send message event");
            }
            other => panic!("unexpected message from server: {other:?}"),
          }
        }
        Err(mpsc::RecvTimeoutError::Timeout) => {
          if Instant::now() >= deadline {
            panic!("network process timed out");
          }
        }
        Err(mpsc::RecvTimeoutError::Disconnected) => break,
      }
    }
  });

  let dom = dom2::Document::new(QuirksMode::NoQuirks);
  let mut host = make_host(dom, "http://example.invalid/")?;

  // Override the default (in-process tungstenite) WebSocket bindings with the IPC-backed version.
  let _ipc_bindings = {
    let window = host.host_mut().window_mut();
    let (vm, realm, heap) = window.vm_realm_and_heap_mut();
    install_window_websocket_ipc_bindings_with_guard::<WindowHostState>(
      vm,
      realm,
      heap,
      WindowWebSocketIpcEnv {
        fetcher: Arc::new(NoFetchResourceFetcher),
        document_url: Some("http://example.invalid/".to_string()),
        cmd_tx,
        event_rx,
      },
    )
    .map_err(|err| Error::Other(err.to_string()))?
  };

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
    ws.onerror = function (_e) {{
      globalThis.__err = "error";
      globalThis.__done = true;
    }};
    ws.onclose = function () {{
      globalThis.__done = true;
    }};
    "#,
  ))?;

  pump_until_done(&mut host, Instant::now() + Duration::from_secs(5))?;

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

  network.join().expect("network thread panicked");
  server.join().expect("server thread panicked");
  Ok(())
}

#[test]
fn websocket_ipc_responds_to_ping_frames() -> Result<()> {
  let Ok(listener) = TcpListener::bind("127.0.0.1:0") else {
    // Some sandboxed CI environments may forbid binding sockets; skip in that case.
    return Ok(());
  };
  listener.set_nonblocking(true).expect("set_nonblocking");
  let addr = listener.local_addr().expect("local_addr");

  let ping_payload: Vec<u8> = b"fastrender-ipc-ping".to_vec();
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

          ws.write_message(tungstenite::protocol::Message::Ping(
            ping_payload_server.clone(),
          ))
            .expect("server ping write failed");

          let read_deadline = Instant::now() + Duration::from_secs(5);
          loop {
            match ws.read_message() {
              Ok(tungstenite::protocol::Message::Pong(payload)) => {
                assert_eq!(payload, ping_payload_server, "pong payload mismatch");
                break;
              }
              Ok(other) => panic!("expected pong, got {other:?}"),
              Err(tungstenite::Error::Io(ref err))
                if matches!(
                  err.kind(),
                  std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
                ) =>
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

  let (cmd_tx, cmd_rx) = mpsc::sync_channel::<WebSocketIpcCommand>(16);
  let (event_tx, event_rx) = mpsc::channel::<WebSocketIpcEvent>();

  let network = std::thread::spawn(move || {
    use tungstenite::client::IntoClientRequest;
    use tungstenite::protocol::Message;

    let connect_deadline = Instant::now() + Duration::from_secs(5);
    let (ws_id, url, protocols) = loop {
      match cmd_rx.recv_timeout(Duration::from_millis(50)) {
        Ok(WebSocketIpcCommand::WebSocket { conn_id, cmd }) => match cmd {
          WebSocketCommand::Connect { params } => break (conn_id, params.url, params.protocols),
          other => panic!("unexpected command before connect: {other:?}"),
        },
        Err(mpsc::RecvTimeoutError::Timeout) => {
          if Instant::now() >= connect_deadline {
            panic!("network process timed out waiting for connect command");
          }
        }
        Err(mpsc::RecvTimeoutError::Disconnected) => return,
      }
    };

    let mut req = url.into_client_request().expect("into_client_request");
    if !protocols.is_empty() {
      let header = protocols.join(", ");
      req
        .headers_mut()
        .insert("Sec-WebSocket-Protocol", header.parse().unwrap());
    }

    let (mut socket, response) = tungstenite::connect(req).expect("connect");
    let selected_protocol =
      validate_ws_subprotocol_handshake_response(&protocols, response.headers()).unwrap_or_default();
    event_tx
      .send(WebSocketIpcEvent::WebSocket {
        conn_id: ws_id,
        event: WebSocketEvent::Open { selected_protocol },
      })
      .expect("send open event");

    // Poll both the command channel and the websocket so we can respond to pings even when JS isn't
    // sending anything.
    let poll_timeout = Duration::from_millis(50);
    match socket.get_ref() {
      tungstenite::stream::MaybeTlsStream::Plain(stream) => {
        let _ = stream.set_read_timeout(Some(poll_timeout));
      }
      tungstenite::stream::MaybeTlsStream::Rustls(stream) => {
        let _ = stream.get_ref().set_read_timeout(Some(poll_timeout));
      }
      #[allow(unreachable_patterns)]
      _ => {}
    }

    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
      // Drain any pending commands.
      loop {
        match cmd_rx.try_recv() {
          Ok(WebSocketIpcCommand::WebSocket { conn_id, cmd }) => {
            assert_eq!(conn_id, ws_id);
            match cmd {
              WebSocketCommand::Close { .. } | WebSocketCommand::Shutdown => {
                let _ = socket.close(None);
                return;
              }
              WebSocketCommand::SendText { .. } | WebSocketCommand::SendBinary { .. } => {
                panic!("send not used by ping test");
              }
              WebSocketCommand::Connect { .. } => panic!("unexpected second connect"),
            }
          }
          Err(mpsc::TryRecvError::Empty) => break,
          Err(mpsc::TryRecvError::Disconnected) => return,
        }
      }

      match socket.read_message() {
        Ok(Message::Ping(payload)) => {
          // RFC 6455: reply to pings with a pong containing the same payload.
          let _ = socket.write_message(Message::Pong(payload));
        }
        Ok(Message::Pong(_)) => {}
        Ok(Message::Close(frame)) => {
          let (code, reason) = frame
            .as_ref()
            .map(|f| (u16::from(f.code), f.reason.to_string()))
            .unwrap_or((1000, "".to_string()));
          let _ = event_tx.send(WebSocketIpcEvent::WebSocket {
            conn_id: ws_id,
            event: WebSocketEvent::Close { code, reason },
          });
          return;
        }
        Ok(Message::Text(text)) => {
          let _ = event_tx.send(WebSocketIpcEvent::WebSocket {
            conn_id: ws_id,
            event: WebSocketEvent::MessageText { text },
          });
        }
        Ok(Message::Binary(data)) => {
          let _ = event_tx.send(WebSocketIpcEvent::WebSocket {
            conn_id: ws_id,
            event: WebSocketEvent::MessageBinary { data },
          });
        }
        Err(tungstenite::Error::Io(ref err))
          if matches!(err.kind(), std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock) => {}
        Err(err) => panic!("network process read failed: {err}"),
        _ => {}
      }

      if Instant::now() >= deadline {
        panic!("network process timed out");
      }
    }
  });

  let dom = dom2::Document::new(QuirksMode::NoQuirks);
  let mut host = make_host(dom, "http://example.invalid/")?;

  let _ipc_bindings = {
    let window = host.host_mut().window_mut();
    let (vm, realm, heap) = window.vm_realm_and_heap_mut();
    install_window_websocket_ipc_bindings_with_guard::<WindowHostState>(
      vm,
      realm,
      heap,
      WindowWebSocketIpcEnv {
        fetcher: Arc::new(NoFetchResourceFetcher),
        document_url: Some("http://example.invalid/".to_string()),
        cmd_tx,
        event_rx,
      },
    )
    .map_err(|err| Error::Other(err.to_string()))?
  };

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

  pump_until_done(&mut host, Instant::now() + Duration::from_secs(5))?;

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

  network.join().expect("network thread panicked");
  server.join().expect("server thread panicked");

  Ok(())
}

#[test]
fn websocket_ipc_rejects_unrequested_protocol_selected_by_server() -> Result<()> {
  let Ok(listener) = TcpListener::bind("127.0.0.1:0") else {
    // Some sandboxed CI environments may forbid binding sockets; skip in that case.
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
          let _ws = tungstenite::accept_hdr(stream, |_req: &Request, mut resp: Response| {
            resp
              .headers_mut()
              .insert("Sec-WebSocket-Protocol", "chat, superchat".parse().unwrap());
            Ok::<_, tungstenite::handshake::server::ErrorResponse>(resp)
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

  let (cmd_tx, cmd_rx) = mpsc::sync_channel::<WebSocketIpcCommand>(16);
  let (event_tx, event_rx) = mpsc::channel::<WebSocketIpcEvent>();

  let network = std::thread::spawn(move || {
    use tungstenite::client::IntoClientRequest;

    let connect_deadline = Instant::now() + Duration::from_secs(5);
    let (ws_id, url, protocols) = loop {
      match cmd_rx.recv_timeout(Duration::from_millis(50)) {
        Ok(WebSocketIpcCommand::WebSocket { conn_id, cmd }) => match cmd {
          WebSocketCommand::Connect { params } => {
            assert!(
              params
                .document_url
                .as_deref()
                .unwrap_or_default()
                .starts_with("http://"),
              "expected http:// document to be treated as an insecure context"
            );
            break (conn_id, params.url, params.protocols);
          }
          other => panic!("unexpected command before connect: {other:?}"),
        },
        Err(mpsc::RecvTimeoutError::Timeout) => {
          if Instant::now() >= connect_deadline {
            panic!("network process timed out waiting for connect command");
          }
        }
        Err(mpsc::RecvTimeoutError::Disconnected) => return,
      }
    };

    let mut req = url.into_client_request().expect("into_client_request");
    if !protocols.is_empty() {
      let header = protocols.join(", ");
      req
        .headers_mut()
        .insert("Sec-WebSocket-Protocol", header.parse().unwrap());
    }

    let (_socket, response) = match tungstenite::connect(req) {
      Ok(pair) => pair,
      Err(_err) => {
        event_tx
          .send(WebSocketIpcEvent::WebSocket {
            conn_id: ws_id,
            event: WebSocketEvent::Error {
              message: Some("invalid websocket subprotocol".to_string()),
            },
          })
          .expect("send error event");
        return;
      }
    };
    if validate_ws_subprotocol_handshake_response(&protocols, response.headers()).is_err() {
      event_tx
        .send(WebSocketIpcEvent::WebSocket {
          conn_id: ws_id,
          event: WebSocketEvent::Error {
            message: Some("invalid websocket subprotocol".to_string()),
          },
        })
        .expect("send error event");
      return;
    }

    panic!("expected invalid protocol handshake to be rejected");
  });

  let dom = dom2::Document::new(QuirksMode::NoQuirks);
  let mut host = make_host(dom, "http://example.invalid/")?;

  let _ipc_bindings = {
    let window = host.host_mut().window_mut();
    let (vm, realm, heap) = window.vm_realm_and_heap_mut();
    install_window_websocket_ipc_bindings_with_guard::<WindowHostState>(
      vm,
      realm,
      heap,
      WindowWebSocketIpcEnv {
        fetcher: Arc::new(NoFetchResourceFetcher),
        document_url: Some("http://example.invalid/".to_string()),
        cmd_tx,
        event_rx,
      },
    )
    .map_err(|err| Error::Other(err.to_string()))?
  };

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

  pump_until_done(&mut host, Instant::now() + Duration::from_secs(5))?;

  assert_eq!(
    get_global_prop_utf8(&mut host, "__opened").as_deref(),
    Some("false")
  );
  assert_eq!(
    get_global_prop_utf8(&mut host, "__error").as_deref(),
    Some("true")
  );
  assert_eq!(
    get_global_prop_utf8(&mut host, "__closed").as_deref(),
    Some("true")
  );

  network.join().expect("network thread panicked");
  server.join().expect("server thread panicked");
  Ok(())
}

#[test]
fn websocket_ipc_protocol_is_set_from_server_handshake_response() -> Result<()> {
  let Ok(listener) = TcpListener::bind("127.0.0.1:0") else {
    // Some sandboxed CI environments may forbid binding sockets; skip in that case.
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
          let _ws = tungstenite::accept_hdr(stream, |_req: &Request, mut resp: Response| {
            resp
              .headers_mut()
              .insert("Sec-WebSocket-Protocol", "superchat".parse().unwrap());
            Ok::<_, tungstenite::handshake::server::ErrorResponse>(resp)
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

  let (cmd_tx, cmd_rx) = mpsc::sync_channel::<WebSocketIpcCommand>(16);
  let (event_tx, event_rx) = mpsc::channel::<WebSocketIpcEvent>();

  let network = std::thread::spawn(move || {
    use tungstenite::client::IntoClientRequest;

    let connect_deadline = Instant::now() + Duration::from_secs(5);
    let (ws_id, url, protocols) = loop {
      match cmd_rx.recv_timeout(Duration::from_millis(50)) {
        Ok(WebSocketIpcCommand::WebSocket { conn_id, cmd }) => match cmd {
          WebSocketCommand::Connect { params } => {
            assert!(
              params
                .document_url
                .as_deref()
                .unwrap_or_default()
                .starts_with("http://"),
              "expected http:// document to be treated as an insecure context"
            );
            break (conn_id, params.url, params.protocols);
          }
          other => panic!("unexpected command before connect: {other:?}"),
        },
        Err(mpsc::RecvTimeoutError::Timeout) => {
          if Instant::now() >= connect_deadline {
            panic!("network process timed out waiting for connect command");
          }
        }
        Err(mpsc::RecvTimeoutError::Disconnected) => return,
      }
    };

    let mut req = url.into_client_request().expect("into_client_request");
    if !protocols.is_empty() {
      let header = protocols.join(", ");
      req
        .headers_mut()
        .insert("Sec-WebSocket-Protocol", header.parse().unwrap());
    }

    let (mut socket, response) = tungstenite::connect(req).expect("connect");
    let selected_protocol =
      validate_ws_subprotocol_handshake_response(&protocols, response.headers()).expect("protocol validate");

    event_tx
      .send(WebSocketIpcEvent::WebSocket {
        conn_id: ws_id,
        event: WebSocketEvent::Open {
          selected_protocol,
        },
      })
      .expect("send open event");

    // Wait for renderer to close.
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
      match cmd_rx.recv_timeout(Duration::from_millis(50)) {
        Ok(WebSocketIpcCommand::WebSocket { conn_id, cmd }) => {
          assert_eq!(conn_id, ws_id);
          match cmd {
            WebSocketCommand::Close { code, reason } => {
              let _ = socket.close(None);
              event_tx
                .send(WebSocketIpcEvent::WebSocket {
                  conn_id: ws_id,
                  event: WebSocketEvent::Close {
                    code: code.unwrap_or(1000),
                    reason: reason.unwrap_or_default(),
                  },
                })
                .expect("send close event");
              break;
            }
            WebSocketCommand::Shutdown => break,
            WebSocketCommand::Connect { .. } => panic!("unexpected second connect"),
            other => panic!("unexpected command: {other:?}"),
          }
        }
        Err(mpsc::RecvTimeoutError::Timeout) => {
          if Instant::now() >= deadline {
            panic!("network process timed out");
          }
        }
        Err(mpsc::RecvTimeoutError::Disconnected) => break,
      }
    }
  });

  let dom = dom2::Document::new(QuirksMode::NoQuirks);
  let mut host = make_host(dom, "http://example.invalid/")?;

  let _ipc_bindings = {
    let window = host.host_mut().window_mut();
    let (vm, realm, heap) = window.vm_realm_and_heap_mut();
    install_window_websocket_ipc_bindings_with_guard::<WindowHostState>(
      vm,
      realm,
      heap,
      WindowWebSocketIpcEnv {
        fetcher: Arc::new(NoFetchResourceFetcher),
        document_url: Some("http://example.invalid/".to_string()),
        cmd_tx,
        event_rx,
      },
    )
    .map_err(|err| Error::Other(err.to_string()))?
  };

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

  pump_until_done(&mut host, Instant::now() + Duration::from_secs(5))?;

  assert_eq!(get_global_prop_utf8(&mut host, "__err").as_deref(), Some(""));
  assert_eq!(
    get_global_prop_utf8(&mut host, "__protocol").as_deref(),
    Some("superchat")
  );

  network.join().expect("network thread panicked");
  server.join().expect("server thread panicked");
  Ok(())
}

#[test]
fn websocket_ipc_rejects_protocol_when_none_were_requested() -> Result<()> {
  let Ok(listener) = TcpListener::bind("127.0.0.1:0") else {
    // Some sandboxed CI environments may forbid binding sockets; skip in that case.
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
          let _ws = tungstenite::accept_hdr(stream, |_req: &Request, mut resp: Response| {
            resp
              .headers_mut()
              .insert("Sec-WebSocket-Protocol", "chat".parse().unwrap());
            Ok::<_, tungstenite::handshake::server::ErrorResponse>(resp)
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

  let (cmd_tx, cmd_rx) = mpsc::sync_channel::<WebSocketIpcCommand>(16);
  let (event_tx, event_rx) = mpsc::channel::<WebSocketIpcEvent>();

  let network = std::thread::spawn(move || {
    use tungstenite::client::IntoClientRequest;

    let connect_deadline = Instant::now() + Duration::from_secs(5);
    let (ws_id, url, protocols) = loop {
      match cmd_rx.recv_timeout(Duration::from_millis(50)) {
        Ok(WebSocketIpcCommand::WebSocket { conn_id, cmd }) => match cmd {
          WebSocketCommand::Connect { params } => {
            assert!(
              params
                .document_url
                .as_deref()
                .unwrap_or_default()
                .starts_with("http://"),
              "expected http:// document to be treated as an insecure context"
            );
            break (conn_id, params.url, params.protocols);
          }
          other => panic!("unexpected command before connect: {other:?}"),
        },
        Err(mpsc::RecvTimeoutError::Timeout) => {
          if Instant::now() >= connect_deadline {
            panic!("network process timed out waiting for connect command");
          }
        }
        Err(mpsc::RecvTimeoutError::Disconnected) => return,
      }
    };

    assert!(protocols.is_empty(), "expected no protocols for this test");

    let mut req = url.into_client_request().expect("into_client_request");
    if !protocols.is_empty() {
      let header = protocols.join(", ");
      req
        .headers_mut()
        .insert("Sec-WebSocket-Protocol", header.parse().unwrap());
    }
    let (_socket, response) = match tungstenite::connect(req) {
      Ok(pair) => pair,
      Err(_err) => {
        event_tx
          .send(WebSocketIpcEvent::WebSocket {
            conn_id: ws_id,
            event: WebSocketEvent::Error {
              message: Some("invalid websocket subprotocol".to_string()),
            },
          })
          .expect("send error event");
        return;
      }
    };
    if validate_ws_subprotocol_handshake_response(&protocols, response.headers()).is_err() {
      event_tx
        .send(WebSocketIpcEvent::WebSocket {
          conn_id: ws_id,
          event: WebSocketEvent::Error {
            message: Some("invalid websocket subprotocol".to_string()),
          },
        })
        .expect("send error event");
      return;
    }

    panic!("expected invalid protocol handshake to be rejected");
  });

  let dom = dom2::Document::new(QuirksMode::NoQuirks);
  let mut host = make_host(dom, "http://example.invalid/")?;

  let _ipc_bindings = {
    let window = host.host_mut().window_mut();
    let (vm, realm, heap) = window.vm_realm_and_heap_mut();
    install_window_websocket_ipc_bindings_with_guard::<WindowHostState>(
      vm,
      realm,
      heap,
      WindowWebSocketIpcEnv {
        fetcher: Arc::new(NoFetchResourceFetcher),
        document_url: Some("http://example.invalid/".to_string()),
        cmd_tx,
        event_rx,
      },
    )
    .map_err(|err| Error::Other(err.to_string()))?
  };

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

  pump_until_done(&mut host, Instant::now() + Duration::from_secs(5))?;

  assert_eq!(
    get_global_prop_utf8(&mut host, "__opened").as_deref(),
    Some("false")
  );
  assert_eq!(
    get_global_prop_utf8(&mut host, "__error").as_deref(),
    Some("true")
  );
  assert_eq!(
    get_global_prop_utf8(&mut host, "__closed").as_deref(),
    Some("true")
  );

  network.join().expect("network thread panicked");
  server.join().expect("server thread panicked");
  Ok(())
}

#[test]
fn websocket_ipc_renderer_rejects_open_event_with_unrequested_protocol() -> Result<()> {
  // Fake renderer<->network IPC channels (no real server connection needed).
  let (cmd_tx, cmd_rx) = mpsc::sync_channel::<WebSocketIpcCommand>(16);
  let (event_tx, event_rx) = mpsc::channel::<WebSocketIpcEvent>();

  // Fake network process: send an Open event with a protocol not in the requested list.
  let network = std::thread::spawn(move || {
    let connect_deadline = Instant::now() + Duration::from_secs(5);
    let ws_id = loop {
      match cmd_rx.recv_timeout(Duration::from_millis(50)) {
        Ok(WebSocketIpcCommand::WebSocket { conn_id, cmd }) => match cmd {
          WebSocketCommand::Connect { params } => {
            assert_eq!(params.protocols, vec!["chat"]);
            break conn_id;
          }
          other => panic!("unexpected command before connect: {other:?}"),
        },
        Err(mpsc::RecvTimeoutError::Timeout) => {
          if Instant::now() >= connect_deadline {
            panic!("network process timed out waiting for connect command");
          }
        }
        Err(mpsc::RecvTimeoutError::Disconnected) => return,
      }
    };

    // Protocol not in requested list => renderer must fail the connection.
    event_tx
      .send(WebSocketIpcEvent::WebSocket {
        conn_id: ws_id,
        event: WebSocketEvent::Open {
          selected_protocol: "superchat".to_string(),
        },
      })
      .expect("send open event");
  });

  let dom = dom2::Document::new(QuirksMode::NoQuirks);
  let mut host = make_host(dom, "http://example.invalid/")?;

  let _ipc_bindings = {
    let window = host.host_mut().window_mut();
    let (vm, realm, heap) = window.vm_realm_and_heap_mut();
    install_window_websocket_ipc_bindings_with_guard::<WindowHostState>(
      vm,
      realm,
      heap,
      WindowWebSocketIpcEnv {
        fetcher: Arc::new(NoFetchResourceFetcher),
        document_url: Some("http://example.invalid/".to_string()),
        cmd_tx,
        event_rx,
      },
    )
    .map_err(|err| Error::Other(err.to_string()))?
  };

  host.exec_script(
    r#"
    globalThis.__done = false;
    globalThis.__opened = false;
    globalThis.__errored = false;
    globalThis.__closed = false;
    globalThis.__protocol = "";
    globalThis.__ready = -1;

    const ws = new WebSocket("ws://example.invalid/", ["chat"]);
    ws.onopen = function () {
      globalThis.__opened = true;
      globalThis.__protocol = ws.protocol;
      globalThis.__done = true;
    };
    ws.onerror = function () {
      globalThis.__errored = true;
      globalThis.__protocol = ws.protocol;
    };
    ws.onclose = function () {
      globalThis.__closed = true;
      globalThis.__protocol = ws.protocol;
      globalThis.__ready = ws.readyState;
      globalThis.__done = true;
    };
    "#,
  )?;

  pump_until_done(&mut host, Instant::now() + Duration::from_secs(5))?;

  assert_eq!(
    get_global_prop_utf8(&mut host, "__opened").as_deref(),
    Some("false")
  );
  assert_eq!(
    get_global_prop_utf8(&mut host, "__errored").as_deref(),
    Some("true")
  );
  assert_eq!(
    get_global_prop_utf8(&mut host, "__closed").as_deref(),
    Some("true")
  );
  assert_eq!(
    get_global_prop_utf8(&mut host, "__protocol").as_deref(),
    Some("")
  );
  assert_eq!(
    get_global_prop_utf8(&mut host, "__ready").as_deref(),
    Some("3")
  );

  network.join().expect("network thread panicked");
  Ok(())
}

#[test]
fn websocket_ipc_renderer_rejects_open_event_protocol_when_none_requested() -> Result<()> {
  let (cmd_tx, cmd_rx) = mpsc::sync_channel::<WebSocketIpcCommand>(16);
  let (event_tx, event_rx) = mpsc::channel::<WebSocketIpcEvent>();

  // Fake network process: claim a protocol was selected even though none were requested.
  let network = std::thread::spawn(move || {
    let connect_deadline = Instant::now() + Duration::from_secs(5);
    let ws_id = loop {
      match cmd_rx.recv_timeout(Duration::from_millis(50)) {
        Ok(WebSocketIpcCommand::WebSocket { conn_id, cmd }) => match cmd {
          WebSocketCommand::Connect { params } => {
            assert!(
              params.protocols.is_empty(),
              "expected no protocols for this test"
            );
            break conn_id;
          }
          other => panic!("unexpected command before connect: {other:?}"),
        },
        Err(mpsc::RecvTimeoutError::Timeout) => {
          if Instant::now() >= connect_deadline {
            panic!("network process timed out waiting for connect command");
          }
        }
        Err(mpsc::RecvTimeoutError::Disconnected) => return,
      }
    };

    event_tx
      .send(WebSocketIpcEvent::WebSocket {
        conn_id: ws_id,
        event: WebSocketEvent::Open {
          selected_protocol: "chat".to_string(),
        },
      })
      .expect("send open event");
  });

  let dom = dom2::Document::new(QuirksMode::NoQuirks);
  let mut host = make_host(dom, "http://example.invalid/")?;

  let _ipc_bindings = {
    let window = host.host_mut().window_mut();
    let (vm, realm, heap) = window.vm_realm_and_heap_mut();
    install_window_websocket_ipc_bindings_with_guard::<WindowHostState>(
      vm,
      realm,
      heap,
      WindowWebSocketIpcEnv {
        fetcher: Arc::new(NoFetchResourceFetcher),
        document_url: Some("http://example.invalid/".to_string()),
        cmd_tx,
        event_rx,
      },
    )
    .map_err(|err| Error::Other(err.to_string()))?
  };

  host.exec_script(
    r#"
    globalThis.__done = false;
    globalThis.__opened = false;
    globalThis.__errored = false;
    globalThis.__closed = false;
    globalThis.__protocol = "";
    globalThis.__ready = -1;

    const ws = new WebSocket("ws://example.invalid/");
    ws.onopen = function () {
      globalThis.__opened = true;
      globalThis.__protocol = ws.protocol;
      globalThis.__done = true;
    };
    ws.onerror = function () {
      globalThis.__errored = true;
      globalThis.__protocol = ws.protocol;
    };
    ws.onclose = function () {
      globalThis.__closed = true;
      globalThis.__protocol = ws.protocol;
      globalThis.__ready = ws.readyState;
      globalThis.__done = true;
    };
    "#,
  )?;

  pump_until_done(&mut host, Instant::now() + Duration::from_secs(5))?;

  assert_eq!(
    get_global_prop_utf8(&mut host, "__opened").as_deref(),
    Some("false")
  );
  assert_eq!(
    get_global_prop_utf8(&mut host, "__errored").as_deref(),
    Some("true")
  );
  assert_eq!(
    get_global_prop_utf8(&mut host, "__closed").as_deref(),
    Some("true")
  );
  assert_eq!(
    get_global_prop_utf8(&mut host, "__protocol").as_deref(),
    Some("")
  );
  assert_eq!(
    get_global_prop_utf8(&mut host, "__ready").as_deref(),
    Some("3")
  );

  network.join().expect("network thread panicked");
  Ok(())
}

#[test]
fn websocket_ipc_send_queue_full_does_not_increase_buffered_amount() -> Result<()> {
  // Small bounded command queue so we can deterministically hit backpressure.
  let (cmd_tx, cmd_rx) = mpsc::sync_channel::<WebSocketIpcCommand>(4);
  let (event_tx, event_rx) = mpsc::channel::<WebSocketIpcEvent>();
  let (stop_tx, stop_rx) = mpsc::channel::<()>();

  // Fake network process: acknowledge connect/open, then stop reading commands so the send queue
  // fills up.
  let network = std::thread::spawn(move || {
    let connect_deadline = Instant::now() + Duration::from_secs(5);
    let ws_id = loop {
      match cmd_rx.recv_timeout(Duration::from_millis(50)) {
        Ok(WebSocketIpcCommand::WebSocket { conn_id, cmd }) => match cmd {
          WebSocketCommand::Connect { .. } => break conn_id,
          other => panic!("unexpected command before connect: {other:?}"),
        },
        Err(mpsc::RecvTimeoutError::Timeout) => {
          if Instant::now() >= connect_deadline {
            panic!("network process timed out waiting for connect command");
          }
        }
        Err(mpsc::RecvTimeoutError::Disconnected) => return,
      }
    };

    event_tx
      .send(WebSocketIpcEvent::WebSocket {
        conn_id: ws_id,
        event: WebSocketEvent::Open {
          selected_protocol: "".to_string(),
        },
      })
      .expect("send open event");

    // Stop reading `cmd_rx` until the test tells us to exit.
    let _ = stop_rx.recv_timeout(Duration::from_secs(5));
  });

  let dom = dom2::Document::new(QuirksMode::NoQuirks);
  let mut host = make_host(dom, "http://example.invalid/")?;

  let _ipc_bindings = {
    let window = host.host_mut().window_mut();
    let (vm, realm, heap) = window.vm_realm_and_heap_mut();
    install_window_websocket_ipc_bindings_with_guard::<WindowHostState>(
      vm,
      realm,
      heap,
      WindowWebSocketIpcEnv {
        fetcher: Arc::new(NoFetchResourceFetcher),
        document_url: Some("http://example.invalid/".to_string()),
        cmd_tx,
        event_rx,
      },
    )
    .map_err(|err| Error::Other(err.to_string()))?
  };

  host.exec_script(
    r#"
    globalThis.__done = false;
    globalThis.__err = "";
    globalThis.__before = 0;
    globalThis.__after = 0;
    globalThis.__ws = new WebSocket("ws://example.invalid/");
    const ws = globalThis.__ws;
    ws.onopen = function () {
      // Try to fill the send queue.
      while (true) {
        const before = ws.bufferedAmount;
        try {
          ws.send("x");
        } catch (e) {
          globalThis.__err = String((e && e.message) || e);
          globalThis.__before = before;
          globalThis.__after = ws.bufferedAmount;
          globalThis.__done = true;
          break;
        }
      }
    };
    ws.onerror = function (_e) {
      globalThis.__err = "error";
      globalThis.__done = true;
    };
    "#,
  )?;

  pump_until_done(&mut host, Instant::now() + Duration::from_secs(5))?;

  assert_eq!(
    get_global_prop_utf8(&mut host, "__err").as_deref(),
    Some("WebSocket send queue is full")
  );
  let before: f64 = get_global_prop_utf8(&mut host, "__before")
    .unwrap_or_default()
    .parse()
    .unwrap_or(-1.0);
  let after: f64 = get_global_prop_utf8(&mut host, "__after")
    .unwrap_or_default()
    .parse()
    .unwrap_or(-1.0);
  assert_eq!(
    before, after,
    "expected bufferedAmount to not change after failed send (before={before}, after={after})"
  );

  // Allow the fake network process to exit and join it so we don't leak threads across tests.
  let _ = stop_tx.send(());
  network.join().expect("network thread panicked");

  Ok(())
}
