use std::collections::HashMap;

/// Stable identifier for a renderer IPC channel connected to the network process.
///
/// The network process may service multiple renderer processes at once; this ID is the key used for
/// per-renderer resource accounting.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct RendererChannelId(pub u64);

/// WebSocket connection identifier allocated by the renderer.
pub type WebSocketConnId = u64;

/// Configuration for [`WebSocketManager`].
#[derive(Debug, Clone, Copy)]
pub struct WebSocketManagerLimits {
  /// Hard cap on concurrently-active WebSockets per renderer.
  pub max_active_per_renderer: usize,
  /// Hard cap on concurrently-active WebSockets across all renderers.
  pub max_active_total: usize,
}

impl Default for WebSocketManagerLimits {
  fn default() -> Self {
    Self {
      // Enough for real pages, while preventing a compromised renderer from exhausting resources.
      max_active_per_renderer: 256,
      // Global backstop in case many renderers are alive.
      max_active_total: 4096,
    }
  }
}

/// Renderer → network process WebSocket commands.
#[derive(Debug, Clone)]
pub enum WebSocketCommand {
  /// Establish a new WebSocket connection.
  Connect { conn_id: WebSocketConnId },
  /// Send a text message over an established WebSocket connection.
  ///
  /// Unknown/closed `conn_id`s are ignored (best-effort) so a compromised renderer cannot crash the
  /// network process by racing teardown.
  SendText {
    conn_id: WebSocketConnId,
    text: String,
  },
  /// Renderer-initiated close (best-effort; the connection remains active until the backend
  /// confirms closure).
  Close { conn_id: WebSocketConnId },
  /// Best-effort abort used during renderer teardown.
  ///
  /// This is treated the same as `Close` today, but is kept distinct because future backends may
  /// prefer an immediate abort (no close handshake) during process shutdown.
  Shutdown { conn_id: WebSocketConnId },
}

/// Network process → renderer WebSocket events.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WebSocketEvent {
  /// WebSocket error notification.
  Error {
    conn_id: WebSocketConnId,
    message: &'static str,
  },
  /// WebSocket close notification.
  Close {
    conn_id: WebSocketConnId,
    code: u16,
    reason: &'static str,
  },
}

/// Sink for emitting [`WebSocketEvent`]s back to a renderer.
pub trait WebSocketEventSink {
  fn send(&mut self, event: WebSocketEvent);
}

/// Backend hook invoked by [`WebSocketManager`] when a connection is accepted.
///
/// In production this is where the network process would spawn the per-connection worker (thread,
/// async task, etc.).
pub trait WebSocketBackend {
  fn connect(&mut self, renderer: RendererChannelId, conn_id: WebSocketConnId);
  fn send_text(&mut self, renderer: RendererChannelId, conn_id: WebSocketConnId, text: String);
  fn request_close(&mut self, renderer: RendererChannelId, conn_id: WebSocketConnId);
}

impl WebSocketBackend for () {
  fn connect(&mut self, _renderer: RendererChannelId, _conn_id: WebSocketConnId) {}
  fn send_text(&mut self, _renderer: RendererChannelId, _conn_id: WebSocketConnId, _text: String) {}
  fn request_close(&mut self, _renderer: RendererChannelId, _conn_id: WebSocketConnId) {}
}

const REJECT_CLOSE_CODE: u16 = 1008; // Policy Violation.
const REJECT_REASON_DUPLICATE_CONN_ID: &str = "websocket conn_id already in use";
const REJECT_REASON_PER_RENDERER: &str = "websocket connection limit exceeded";
const REJECT_REASON_GLOBAL: &str = "global websocket connection limit exceeded";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WebSocketConnState {
  Open,
  Closing,
}

/// Renderer-side event router for WebSocket IPC.
///
/// The renderer may observe late events for connections it has already torn down (e.g. the network
/// process sends a `Close` after the renderer dropped its local handle). These must be treated as
/// benign races and ignored.
#[derive(Debug, Default)]
pub struct RendererWebSocketBackend {
  conns: HashMap<WebSocketConnId, ()>,
}

impl RendererWebSocketBackend {
  pub fn register(&mut self, conn_id: WebSocketConnId) {
    self.conns.insert(conn_id, ());
  }

  pub fn unregister(&mut self, conn_id: WebSocketConnId) {
    self.conns.remove(&conn_id);
  }

  /// Returns `true` when the event is for a known connection.
  pub fn handle_event(&mut self, event: &WebSocketEvent) -> bool {
    let conn_id = match *event {
      WebSocketEvent::Error { conn_id, .. } => conn_id,
      WebSocketEvent::Close { conn_id, .. } => conn_id,
    };
    self.conns.contains_key(&conn_id)
  }
}

/// Tracks active WebSocket connections per renderer IPC channel and enforces hard caps.
///
/// This is a defense-in-depth mechanism: a compromised renderer must not be able to exhaust
/// resources in the (more privileged) network process by opening unbounded WebSockets.
pub struct WebSocketManager<B: WebSocketBackend> {
  limits: WebSocketManagerLimits,
  backend: B,
  active_total: usize,
  active_connections: HashMap<RendererChannelId, HashMap<WebSocketConnId, WebSocketConnState>>,
}

impl<B: WebSocketBackend> WebSocketManager<B> {
  pub fn new(limits: WebSocketManagerLimits, backend: B) -> Self {
    Self {
      limits,
      backend,
      active_total: 0,
      active_connections: HashMap::new(),
    }
  }

  pub fn limits(&self) -> WebSocketManagerLimits {
    self.limits
  }

  pub fn active_total(&self) -> usize {
    self.active_total
  }

  pub fn active_for_renderer(&self, renderer: RendererChannelId) -> usize {
    self
      .active_connections
      .get(&renderer)
      .map(|m| m.len())
      .unwrap_or(0)
  }

  pub fn tracked_connection_count(&self) -> usize {
    self.active_total
  }

  pub fn backend(&self) -> &B {
    &self.backend
  }

  pub fn backend_mut(&mut self) -> &mut B {
    &mut self.backend
  }

  /// Handle a renderer-issued command.
  pub fn handle_command(
    &mut self,
    renderer: RendererChannelId,
    sink: &mut dyn WebSocketEventSink,
    cmd: WebSocketCommand,
  ) {
    match cmd {
      WebSocketCommand::Connect { conn_id } => self.handle_connect(renderer, conn_id, sink),
      WebSocketCommand::SendText { conn_id, text } => {
        let Some(state) = self
          .active_connections
          .get(&renderer)
          .and_then(|m| m.get(&conn_id))
        else {
          return;
        };
        if *state == WebSocketConnState::Open {
          self.backend.send_text(renderer, conn_id, text);
        }
      }
      WebSocketCommand::Close { conn_id } => {
        // Best-effort: the connection remains active until `on_connection_closed` is called.
        self.request_close_if_tracked(renderer, conn_id);
      }
      WebSocketCommand::Shutdown { conn_id } => {
        // Treat unknown conn_ids as a benign race during shutdown.
        self.request_close_if_tracked(renderer, conn_id);
      }
    }
  }

  fn handle_connect(
    &mut self,
    renderer: RendererChannelId,
    conn_id: WebSocketConnId,
    sink: &mut dyn WebSocketEventSink,
  ) {
    // Reject duplicate Connects deterministically without touching the existing connection state.
    //
    // This prevents a compromised renderer from reusing conn_id values to override an existing
    // connection entry (which would risk misdelivering events to the wrong connection / UAF-style
    // logic bugs).
    if self
      .active_connections
      .get(&renderer)
      .map_or(false, |m| m.contains_key(&conn_id))
    {
      self.reject_connect(conn_id, REJECT_REASON_DUPLICATE_CONN_ID, sink);
      return;
    }

    let renderer_active = self.active_for_renderer(renderer);
    if renderer_active >= self.limits.max_active_per_renderer {
      self.reject_connect(conn_id, REJECT_REASON_PER_RENDERER, sink);
      return;
    }

    if self.active_total >= self.limits.max_active_total {
      self.reject_connect(conn_id, REJECT_REASON_GLOBAL, sink);
      return;
    }

    // Accept: record counts before invoking backend so failure paths cannot underflow.
    self
      .active_connections
      .entry(renderer)
      .or_default()
      .insert(conn_id, WebSocketConnState::Open);
    self.active_total = self.active_total.saturating_add(1);

    self.backend.connect(renderer, conn_id);
  }

  fn reject_connect(&mut self, conn_id: WebSocketConnId, reason: &'static str, sink: &mut dyn WebSocketEventSink) {
    // Deterministic rejection: Error + Close (no background tasks spawned).
    sink.send(WebSocketEvent::Error {
      conn_id,
      message: reason,
    });
    sink.send(WebSocketEvent::Close {
      conn_id,
      code: REJECT_CLOSE_CODE,
      reason,
    });
  }

  /// Notify the manager that a WebSocket has fully closed.
  ///
  /// The network backend should call this exactly once per accepted connection.
  pub fn on_connection_closed(&mut self, renderer: RendererChannelId, conn_id: WebSocketConnId) {
    let Some(renderer_conns) = self.active_connections.get_mut(&renderer) else {
      return;
    };
    if renderer_conns.remove(&conn_id).is_none() {
      return;
    }
    if renderer_conns.is_empty() {
      self.active_connections.remove(&renderer);
    }

    self.active_total = self.active_total.saturating_sub(1);
  }

  /// Drop all state associated with a renderer IPC channel (e.g. renderer crashed/disconnected).
  pub fn on_renderer_disconnected(&mut self, renderer: RendererChannelId) {
    let Some(conns) = self.active_connections.remove(&renderer) else {
      return;
    };
    // Drop all conn_ids first so a reused `RendererChannelId` cannot reference stale connections.
    let removed = conns.len();
    self.active_total = self.active_total.saturating_sub(removed);
  }

  fn request_close_if_tracked(&mut self, renderer: RendererChannelId, conn_id: WebSocketConnId) {
    let Some(state) = self
      .active_connections
      .get_mut(&renderer)
      .and_then(|m| m.get_mut(&conn_id))
    else {
      return;
    };
    if *state == WebSocketConnState::Closing {
      return;
    }
    *state = WebSocketConnState::Closing;
    self.backend.request_close(renderer, conn_id);
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[derive(Default)]
  struct FakeSink {
    events: Vec<WebSocketEvent>,
  }

  impl WebSocketEventSink for FakeSink {
    fn send(&mut self, event: WebSocketEvent) {
      self.events.push(event);
    }
  }

  #[derive(Default)]
  struct CountingBackend {
    connect_calls: Vec<(RendererChannelId, WebSocketConnId)>,
    send_calls: Vec<(RendererChannelId, WebSocketConnId, String)>,
    close_calls: Vec<(RendererChannelId, WebSocketConnId)>,
  }

  impl WebSocketBackend for CountingBackend {
    fn connect(&mut self, renderer: RendererChannelId, conn_id: WebSocketConnId) {
      self.connect_calls.push((renderer, conn_id));
    }

    fn send_text(&mut self, renderer: RendererChannelId, conn_id: WebSocketConnId, text: String) {
      self.send_calls.push((renderer, conn_id, text));
    }

    fn request_close(&mut self, renderer: RendererChannelId, conn_id: WebSocketConnId) {
      self.close_calls.push((renderer, conn_id));
    }
  }

  #[test]
  fn per_renderer_cap_rejects_and_decrements_on_close() {
    let limits = WebSocketManagerLimits {
      max_active_per_renderer: 2,
      max_active_total: 100,
    };
    let backend = CountingBackend::default();
    let mut mgr = WebSocketManager::new(limits, backend);
    let mut sink = FakeSink::default();
    let renderer = RendererChannelId(1);

    mgr.handle_command(renderer, &mut sink, WebSocketCommand::Connect { conn_id: 10 });
    mgr.handle_command(renderer, &mut sink, WebSocketCommand::Connect { conn_id: 11 });
    assert_eq!(mgr.active_for_renderer(renderer), 2);
    assert_eq!(mgr.active_total(), 2);
    assert_eq!(mgr.tracked_connection_count(), 2);
    assert_eq!(mgr.backend().connect_calls.len(), 2);
    assert!(sink.events.is_empty());

    mgr.handle_command(renderer, &mut sink, WebSocketCommand::Connect { conn_id: 12 });
    assert_eq!(mgr.active_for_renderer(renderer), 2);
    assert_eq!(mgr.active_total(), 2);
    assert_eq!(mgr.tracked_connection_count(), 2);
    assert_eq!(mgr.backend().connect_calls.len(), 2);
    assert_eq!(
      sink.events,
      vec![
        WebSocketEvent::Error {
          conn_id: 12,
          message: REJECT_REASON_PER_RENDERER,
        },
        WebSocketEvent::Close {
          conn_id: 12,
          code: REJECT_CLOSE_CODE,
          reason: REJECT_REASON_PER_RENDERER,
        },
      ]
    );

    sink.events.clear();

    mgr.on_connection_closed(renderer, 10);
    assert_eq!(mgr.active_for_renderer(renderer), 1);
    assert_eq!(mgr.active_total(), 1);
    assert_eq!(mgr.tracked_connection_count(), 1);

    mgr.handle_command(renderer, &mut sink, WebSocketCommand::Connect { conn_id: 13 });
    assert_eq!(mgr.active_for_renderer(renderer), 2);
    assert_eq!(mgr.active_total(), 2);
    assert_eq!(mgr.tracked_connection_count(), 2);
    assert_eq!(mgr.backend().connect_calls.len(), 3);
    assert!(sink.events.is_empty());
  }

  #[test]
  fn global_cap_is_enforced() {
    let limits = WebSocketManagerLimits {
      max_active_per_renderer: 10,
      max_active_total: 2,
    };
    let backend = CountingBackend::default();
    let mut mgr = WebSocketManager::new(limits, backend);
    let mut sink = FakeSink::default();

    let a = RendererChannelId(1);
    let b = RendererChannelId(2);
    mgr.handle_command(a, &mut sink, WebSocketCommand::Connect { conn_id: 1 });
    mgr.handle_command(b, &mut sink, WebSocketCommand::Connect { conn_id: 2 });
    assert_eq!(mgr.active_total(), 2);
    assert_eq!(mgr.backend().connect_calls.len(), 2);
    assert!(sink.events.is_empty());

    mgr.handle_command(a, &mut sink, WebSocketCommand::Connect { conn_id: 3 });
    assert_eq!(mgr.active_total(), 2);
    assert_eq!(mgr.backend().connect_calls.len(), 2);
    assert_eq!(
      sink.events,
      vec![
        WebSocketEvent::Error {
          conn_id: 3,
          message: REJECT_REASON_GLOBAL,
        },
        WebSocketEvent::Close {
          conn_id: 3,
          code: REJECT_CLOSE_CODE,
          reason: REJECT_REASON_GLOBAL,
        },
      ]
    );
  }

  #[test]
  fn spam_connect_over_limit_does_not_grow_state() {
    struct CountingSink {
      error: usize,
      close: usize,
    }

    impl WebSocketEventSink for CountingSink {
      fn send(&mut self, event: WebSocketEvent) {
        match event {
          WebSocketEvent::Error { .. } => self.error += 1,
          WebSocketEvent::Close { .. } => self.close += 1,
        }
      }
    }

    let limits = WebSocketManagerLimits {
      max_active_per_renderer: 1,
      max_active_total: 1,
    };
    let backend = CountingBackend::default();
    let mut mgr = WebSocketManager::new(limits, backend);
    let mut sink = CountingSink { error: 0, close: 0 };
    let renderer = RendererChannelId(1);

    mgr.handle_command(renderer, &mut sink, WebSocketCommand::Connect { conn_id: 1 });
    assert_eq!(mgr.active_total(), 1);
    assert_eq!(mgr.tracked_connection_count(), 1);
    assert_eq!(mgr.backend().connect_calls.len(), 1);

    for i in 2..10_000u64 {
      mgr.handle_command(renderer, &mut sink, WebSocketCommand::Connect { conn_id: i });
    }

    assert_eq!(mgr.active_total(), 1);
    assert_eq!(mgr.tracked_connection_count(), 1);
    assert_eq!(mgr.backend().connect_calls.len(), 1);
    assert_eq!(sink.error, 9_998);
    assert_eq!(sink.close, 9_998);
  }

  #[test]
  fn duplicate_conn_id_is_rejected_deterministically() {
    let limits = WebSocketManagerLimits {
      max_active_per_renderer: 10,
      max_active_total: 10,
    };
    let backend = CountingBackend::default();
    let mut mgr = WebSocketManager::new(limits, backend);
    let mut sink = FakeSink::default();
    let renderer = RendererChannelId(1);

    mgr.handle_command(renderer, &mut sink, WebSocketCommand::Connect { conn_id: 10 });
    assert_eq!(mgr.active_for_renderer(renderer), 1);
    assert_eq!(mgr.backend().connect_calls.len(), 1);
    assert!(sink.events.is_empty());

    mgr.handle_command(renderer, &mut sink, WebSocketCommand::Connect { conn_id: 10 });
    assert_eq!(mgr.active_for_renderer(renderer), 1);
    assert_eq!(mgr.backend().connect_calls.len(), 1);
    assert_eq!(
      sink.events,
      vec![
        WebSocketEvent::Error {
          conn_id: 10,
          message: REJECT_REASON_DUPLICATE_CONN_ID,
        },
        WebSocketEvent::Close {
          conn_id: 10,
          code: REJECT_CLOSE_CODE,
          reason: REJECT_REASON_DUPLICATE_CONN_ID,
        },
      ]
    );
  }

  #[test]
  fn send_unknown_conn_id_is_ignored() {
    let limits = WebSocketManagerLimits {
      max_active_per_renderer: 10,
      max_active_total: 10,
    };
    let backend = CountingBackend::default();
    let mut mgr = WebSocketManager::new(limits, backend);
    let mut sink = FakeSink::default();
    let renderer = RendererChannelId(1);

    mgr.handle_command(
      renderer,
      &mut sink,
      WebSocketCommand::SendText {
        conn_id: 999,
        text: "hello".to_string(),
      },
    );
    assert!(sink.events.is_empty());
    assert!(mgr.backend().send_calls.is_empty());
  }

  #[test]
  fn renderer_backend_drops_unknown_conn_id_events() {
    let mut backend = RendererWebSocketBackend::default();
    backend.register(1);

    let unknown = WebSocketEvent::Close {
      conn_id: 2,
      code: 1000,
      reason: "",
    };
    assert!(
      !backend.handle_event(&unknown),
      "expected unknown conn_id events to be dropped"
    );

    let known = WebSocketEvent::Close {
      conn_id: 1,
      code: 1000,
      reason: "",
    };
    assert!(backend.handle_event(&known));
  }
}
