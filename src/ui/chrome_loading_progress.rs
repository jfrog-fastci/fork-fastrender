/// Compute chrome loading progress for a tab given its `loading` flag and monotonic `load_progress`
/// state.
///
/// Returns:
/// - `None` when `loading` is false (no progress indicator should be shown).
/// - `Some(0.0)` when `loading` is true but the worker hasn't reported a stage yet.
/// - `Some(…)` in `[0.0, 1.0]` otherwise.
#[must_use]
pub fn chrome_loading_progress(loading: bool, load_progress: Option<f32>) -> Option<f32> {
  if !loading {
    return None;
  }
  let progress = load_progress
    .filter(|p| p.is_finite())
    .map(|p| p.clamp(0.0, 1.0))
    .unwrap_or(0.0);
  Some(progress)
}
