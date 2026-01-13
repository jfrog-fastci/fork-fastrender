//! Components intended to run in a dedicated network process.
//!
//! The multiprocess architecture is still under construction, but we keep network-facing state
//! (WebSockets, HTTP fetch, etc.) behind explicit managers so we can apply hard resource caps even
//! when the renderer is compromised.

pub mod websocket_manager;

