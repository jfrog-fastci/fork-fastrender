use crate::render_control::StageHeartbeat;

/// Convert a render pipeline stage heartbeat into a monotonic loading progress fraction.
///
/// This is intended for lightweight chrome loading indicators (e.g. an address bar "progress
/// line"). It is deterministic and does not depend on wall-clock timings.
#[must_use]
pub fn progress_for_stage(stage: StageHeartbeat) -> f32 {
  stage.loading_progress()
}

/// Compute chrome loading progress for a tab given its `loading` flag and last known stage.
///
/// Returns:
/// - `None` when `loading` is false (no progress indicator should be shown).
/// - `Some(0.0)` when `loading` is true but the worker hasn't reported a stage yet.
/// - `Some(…)` in `[0.0, 1.0]` otherwise.
#[must_use]
pub fn chrome_loading_progress(loading: bool, stage: Option<StageHeartbeat>) -> Option<f32> {
  if !loading {
    return None;
  }
  Some(stage.map_or(0.0, progress_for_stage))
}

