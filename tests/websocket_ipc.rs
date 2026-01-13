use fastrender::dom2;
use fastrender::js::{
  install_window_websocket_ipc_bindings_with_guard, RunLimits, WebSocketIpcCommand, WebSocketIpcEvent,
  WindowHost, WindowHostState, WindowWebSocketIpcEnv,
};
use fastrender::{Error, Result};
use selectors::context::QuirksMode;
use std::net::TcpListener;
use std::sync::mpsc;
use std::time::{Duration, Instant};
use vm_js::{PropertyKey, Value};

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
        Ok(WebSocketIpcCommand::Connect { ws_id, url, protocols }) => break (ws_id, url, protocols),
        Ok(other) => panic!("unexpected command before connect: {other:?}"),
        Err(mpsc::RecvTimeoutError::Timeout) => {
          if Instant::now() >= connect_deadline {
            panic!("network process timed out waiting for connect command");
          }
        }
        Err(mpsc::RecvTimeoutError::Disconnected) => return,
      }
    };

    let mut req = url.into_client_request().expect("into_client_request");
    if let Some(header) = protocols.as_deref() {
      req
        .headers_mut()
        .insert("Sec-WebSocket-Protocol", header.parse().unwrap());
    }

    let (mut socket, response) = tungstenite::connect(req).expect("connect");
    let selected_protocol = response
      .headers()
      .get("Sec-WebSocket-Protocol")
      .and_then(|h| h.to_str().ok())
      .unwrap_or("")
      .to_string();
    event_tx
      .send(WebSocketIpcEvent::Open {
        ws_id,
        protocol: selected_protocol,
      })
      .expect("send open event");

    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
      match cmd_rx.recv_timeout(Duration::from_millis(50)) {
        Ok(WebSocketIpcCommand::SendText { ws_id: id, text }) => {
          assert_eq!(id, ws_id);
          let len = text.as_bytes().len();
          socket.write_message(Message::Text(text)).expect("write");
          event_tx
            .send(WebSocketIpcEvent::Sent { ws_id, amount: len })
            .expect("send ack");
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
                .send(WebSocketIpcEvent::MessageText { ws_id, text })
                .expect("send message event");
            }
            other => panic!("unexpected message from server: {other:?}"),
          }
        }
        Ok(WebSocketIpcCommand::SendBinary { .. }) => {
          panic!("binary send not used by this test");
        }
        Ok(WebSocketIpcCommand::Close { ws_id: id, code, reason }) => {
          assert_eq!(id, ws_id);
          let _ = socket.close(None);
          event_tx
            .send(WebSocketIpcEvent::Close {
              ws_id,
              code: code.unwrap_or(1000),
              reason: reason.unwrap_or_default(),
            })
            .expect("send close event");
          break;
        }
        Ok(WebSocketIpcCommand::Connect { .. }) => panic!("unexpected second connect"),
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
  let mut host = WindowHost::new(dom, "https://example.invalid/")?;

  // Override the default (in-process tungstenite) WebSocket bindings with the IPC-backed version.
  let _ipc_bindings = {
    let window = host.host_mut().window_mut();
    let (vm, realm, heap) = window.vm_realm_and_heap_mut();
    install_window_websocket_ipc_bindings_with_guard::<WindowHostState>(
      vm,
      realm,
      heap,
      WindowWebSocketIpcEnv {
        document_url: Some("https://example.invalid/".to_string()),
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
        Ok(WebSocketIpcCommand::Connect { ws_id, .. }) => break ws_id,
        Ok(other) => panic!("unexpected command before connect: {other:?}"),
        Err(mpsc::RecvTimeoutError::Timeout) => {
          if Instant::now() >= connect_deadline {
            panic!("network process timed out waiting for connect command");
          }
        }
        Err(mpsc::RecvTimeoutError::Disconnected) => return,
      }
    };

    event_tx
      .send(WebSocketIpcEvent::Open {
        ws_id,
        protocol: "".to_string(),
      })
      .expect("send open event");

    // Stop reading `cmd_rx` until the test tells us to exit.
    let _ = stop_rx.recv_timeout(Duration::from_secs(5));
  });

  let dom = dom2::Document::new(QuirksMode::NoQuirks);
  let mut host = WindowHost::new(dom, "https://example.invalid/")?;

  let _ipc_bindings = {
    let window = host.host_mut().window_mut();
    let (vm, realm, heap) = window.vm_realm_and_heap_mut();
    install_window_websocket_ipc_bindings_with_guard::<WindowHostState>(
      vm,
      realm,
      heap,
      WindowWebSocketIpcEnv {
        document_url: Some("https://example.invalid/".to_string()),
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
