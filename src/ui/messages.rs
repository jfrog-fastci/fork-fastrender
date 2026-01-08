use crate::render_control::StageHeartbeat;

/// Identifier for a browser UI tab.
///
/// This is kept as a thin wrapper to avoid mixing tab ids with other identifiers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TabId(pub u64);

/// Messages sent from the render worker to the UI thread.
#[derive(Debug, Clone)]
pub enum WorkerToUi {
  /// Coarse-grained stage heartbeat emitted while preparing or painting a document.
  Stage { tab_id: TabId, stage: StageHeartbeat },
}

