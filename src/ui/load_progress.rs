#[derive(Debug, Clone, Copy, PartialEq)]
pub enum LoadProgressIndicator {
  Determinate { progress: f32 },
  Indeterminate,
}

pub fn load_progress_indicator(
  loading: bool,
  progress: Option<f32>,
) -> Option<LoadProgressIndicator> {
  if !loading {
    return None;
  }
  let p = progress.filter(|p| p.is_finite());
  Some(match p {
    // Treat `0.0` as "no meaningful progress yet" so chrome can show an indeterminate animation
    // immediately after navigation starts.
    Some(p) if p > 0.0 => LoadProgressIndicator::Determinate {
      progress: p.clamp(0.0, 1.0),
    },
    _ => LoadProgressIndicator::Indeterminate,
  })
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn progress_line_hidden_when_not_loading() {
    assert_eq!(load_progress_indicator(false, None), None);
    assert_eq!(load_progress_indicator(false, Some(0.5)), None);
  }

  #[test]
  fn progress_line_shows_indeterminate_when_loading_but_progress_unknown() {
    assert_eq!(
      load_progress_indicator(true, None),
      Some(LoadProgressIndicator::Indeterminate)
    );
  }

  #[test]
  fn progress_line_shows_determinate_when_loading_and_progress_known() {
    assert_eq!(
      load_progress_indicator(true, Some(0.25)),
      Some(LoadProgressIndicator::Determinate { progress: 0.25 })
    );
  }

  #[test]
  fn determinate_progress_is_clamped() {
    assert_eq!(
      load_progress_indicator(true, Some(-1.0)),
      Some(LoadProgressIndicator::Indeterminate)
    );
    assert_eq!(
      load_progress_indicator(true, Some(5.0)),
      Some(LoadProgressIndicator::Determinate { progress: 1.0 })
    );
  }

  #[test]
  fn zero_progress_is_treated_as_indeterminate() {
    assert_eq!(
      load_progress_indicator(true, Some(0.0)),
      Some(LoadProgressIndicator::Indeterminate)
    );
  }
}
