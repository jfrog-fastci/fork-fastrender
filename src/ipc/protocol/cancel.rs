use serde::{Deserialize, Serialize};

/// Snapshot of cooperative cancellation generations that can be sent over IPC.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CancelGensSnapshot {
  pub nav: u64,
  pub paint: u64,
}

/// High-level cancellation scope.
///
/// - `Nav` cancels both prepare and paint work (navigation changes).
/// - `Paint` cancels paint work only (viewport/input changes).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub enum CancelScope {
  Nav,
  Paint,
}
