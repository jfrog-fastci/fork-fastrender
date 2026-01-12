use crate::render_control::StageHeartbeat;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PageLoadingUiKind {
  /// No special page-area loading UI is needed.
  None,
  /// No frame exists yet for the active tab, so the UI should show a dedicated loading state
  /// (skeleton/spinner) in the content area.
  Initial,
  /// A frame exists, but a new navigation is loading. The UI should keep showing the last frame
  /// while overlaying a subtle loading scrim/spinner.
  Overlay,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PageLoadingUiDecision {
  pub kind: PageLoadingUiKind,
  /// Primary status line shown in the content-area loading UI.
  pub headline: Option<&'static str>,
  /// Optional additional detail line (kept short; may be omitted to avoid clutter).
  pub detail: Option<&'static str>,
  /// Whether the UI should draw lightweight skeleton blocks in the background.
  pub show_skeleton: bool,
  /// Whether pointer events should be intercepted instead of being forwarded to the page.
  pub intercept_pointer_events: bool,
}

fn stage_detail(stage: StageHeartbeat) -> Option<&'static str> {
  Some(match stage {
    // Fetch / setup
    StageHeartbeat::ReadCache | StageHeartbeat::FollowRedirects => "Fetching…",
    // Parsing + scripting
    StageHeartbeat::CssInline | StageHeartbeat::DomParse => "Parsing…",
    StageHeartbeat::Script => "Running scripts…",
    // Styling / layout
    StageHeartbeat::CssParse | StageHeartbeat::Cascade => "Styling…",
    StageHeartbeat::BoxTree | StageHeartbeat::Layout => "Laying out…",
    // Rasterization
    StageHeartbeat::PaintBuild | StageHeartbeat::PaintRasterize => "Rendering…",
    StageHeartbeat::Done => return None,
  })
}

pub fn decide_page_loading_ui(
  has_frame: bool,
  loading: bool,
  stage: Option<StageHeartbeat>,
) -> PageLoadingUiDecision {
  if !has_frame {
    return PageLoadingUiDecision {
      kind: PageLoadingUiKind::Initial,
      headline: Some(if loading {
        "Loading…"
      } else {
        "Waiting for first frame…"
      }),
      detail: stage.and_then(stage_detail).filter(|_| loading),
      show_skeleton: loading,
      intercept_pointer_events: true,
    };
  }

  if loading {
    return PageLoadingUiDecision {
      kind: PageLoadingUiKind::Overlay,
      headline: None,
      detail: None,
      show_skeleton: false,
      intercept_pointer_events: true,
    };
  }

  PageLoadingUiDecision {
    kind: PageLoadingUiKind::None,
    headline: None,
    detail: None,
    show_skeleton: false,
    intercept_pointer_events: false,
  }
}

#[cfg(test)]
mod tests {
  use super::{decide_page_loading_ui, PageLoadingUiKind};
  use crate::render_control::StageHeartbeat;

  #[test]
  fn initial_state_shows_loading_ui_when_no_frame() {
    let decision = decide_page_loading_ui(false, true, Some(StageHeartbeat::DomParse));
    assert_eq!(decision.kind, PageLoadingUiKind::Initial);
    assert_eq!(decision.headline, Some("Loading…"));
    assert_eq!(decision.detail, Some("Parsing…"));
    assert!(decision.show_skeleton);
    assert!(decision.intercept_pointer_events);
  }

  #[test]
  fn waiting_state_shows_first_frame_message_when_not_loading() {
    let decision = decide_page_loading_ui(false, false, None);
    assert_eq!(decision.kind, PageLoadingUiKind::Initial);
    assert_eq!(decision.headline, Some("Waiting for first frame…"));
    assert_eq!(decision.detail, None);
    assert!(!decision.show_skeleton);
    assert!(decision.intercept_pointer_events);
  }

  #[test]
  fn overlay_state_intercepts_pointer_when_frame_exists_and_loading() {
    let decision = decide_page_loading_ui(true, true, Some(StageHeartbeat::Layout));
    assert_eq!(decision.kind, PageLoadingUiKind::Overlay);
    assert!(decision.intercept_pointer_events);
    assert_eq!(decision.headline, None);
    assert_eq!(decision.detail, None);
  }

  #[test]
  fn no_loading_ui_when_frame_exists_and_not_loading() {
    let decision = decide_page_loading_ui(true, false, Some(StageHeartbeat::Done));
    assert_eq!(decision.kind, PageLoadingUiKind::None);
    assert!(!decision.intercept_pointer_events);
  }
}

