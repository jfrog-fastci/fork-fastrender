use std::collections::{HashMap, HashSet};

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
  /// Renderer-initiated close (best-effort; the connection remains active until the backend
  /// confirms closure).
  Close { conn_id: WebSocketConnId },
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
  fn request_close(&mut self, renderer: RendererChannelId, conn_id: WebSocketConnId);
}

impl WebSocketBackend for () {
  fn connect(&mut self, _renderer: RendererChannelId, _conn_id: WebSocketConnId) {}
  fn request_close(&mut self, _renderer: RendererChannelId, _conn_id: WebSocketConnId) {}
}

const REJECT_CLOSE_CODE: u16 = 1008; // Policy Violation.
const REJECT_REASON_PER_RENDERER: &str = "websocket connection limit exceeded";
const REJECT_REASON_GLOBAL: &str = "global websocket connection limit exceeded";

/// Tracks active WebSocket connections per renderer IPC channel and enforces hard caps.
///
/// This is a defense-in-depth mechanism: a compromised renderer must not be able to exhaust
/// resources in the (more privileged) network process by opening unbounded WebSockets.
pub struct WebSocketManager<B: WebSocketBackend> {
  limits: WebSocketManagerLimits,
  backend: B,
  active_total: usize,
  active_per_renderer: HashMap<RendererChannelId, usize>,
  active_connections: HashSet<(RendererChannelId, WebSocketConnId)>,
}

impl<B: WebSocketBackend> WebSocketManager<B> {
  pub fn new(limits: WebSocketManagerLimits, backend: B) -> Self {
    Self {
      limits,
      backend,
      active_total: 0,
      active_per_renderer: HashMap::new(),
      active_connections: HashSet::new(),
    }
  }

  pub fn limits(&self) -> WebSocketManagerLimits {
    self.limits
  }

  pub fn active_total(&self) -> usize {
    self.active_total
  }

  pub fn active_for_renderer(&self, renderer: RendererChannelId) -> usize {
    self.active_per_renderer.get(&renderer).copied().unwrap_or(0)
  }

  pub fn tracked_connection_count(&self) -> usize {
    self.active_connections.len()
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
      WebSocketCommand::Close { conn_id } => {
        // Best-effort: the connection remains active until `on_connection_closed` is called.
        if self.active_connections.contains(&(renderer, conn_id)) {
          self.backend.request_close(renderer, conn_id);
        }
      }
    }
  }

  fn handle_connect(
    &mut self,
    renderer: RendererChannelId,
    conn_id: WebSocketConnId,
    sink: &mut dyn WebSocketEventSink,
  ) {
    // Ignore duplicate Connects for an already-active conn_id; this avoids active-count drift.
    if self.active_connections.contains(&(renderer, conn_id)) {
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
    self.active_connections.insert((renderer, conn_id));
    self.active_total = self.active_total.saturating_add(1);
    self
      .active_per_renderer
      .insert(renderer, renderer_active.saturating_add(1));

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
    if !self.active_connections.remove(&(renderer, conn_id)) {
      return;
    }

    self.active_total = self.active_total.saturating_sub(1);
    let Some(current) = self.active_per_renderer.get_mut(&renderer) else {
      return;
    };
    *current = current.saturating_sub(1);
    if *current == 0 {
      self.active_per_renderer.remove(&renderer);
    }
  }

  /// Drop all state associated with a renderer IPC channel (e.g. renderer crashed/disconnected).
  pub fn on_renderer_disconnected(&mut self, renderer: RendererChannelId) {
    let conns: Vec<WebSocketConnId> = self
      .active_connections
      .iter()
      .filter_map(|(r, conn)| if *r == renderer { Some(*conn) } else { None })
      .collect();
    for conn_id in conns {
      self.on_connection_closed(renderer, conn_id);
    }
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
    close_calls: Vec<(RendererChannelId, WebSocketConnId)>,
  }

  impl WebSocketBackend for CountingBackend {
    fn connect(&mut self, renderer: RendererChannelId, conn_id: WebSocketConnId) {
      self.connect_calls.push((renderer, conn_id));
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
}

