//! IPC protocol between the renderer process and the network process.
//!
//! The renderer process is untrusted. All renderer-supplied messages must be explicitly validated
//! by the receiving network process (see [`crate::ipc::websocket`] validation helpers).

use serde::{Deserialize, Serialize};

use super::websocket::{WebSocketCommand, WebSocketEvent};

/// Messages sent from the (untrusted) renderer process to the (trusted) network process.
///
/// Note: `conn_id` values are opaque and untrusted. The network process must scope `conn_id` to the
/// renderer channel that issued it (i.e. do not allow one renderer to affect another renderer's
/// connections).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum RendererToNetwork {
  WebSocket { conn_id: u64, cmd: WebSocketCommand },
}

/// Messages sent from the network process to the renderer process.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum NetworkToRenderer {
  WebSocket { conn_id: u64, event: WebSocketEvent },
}

