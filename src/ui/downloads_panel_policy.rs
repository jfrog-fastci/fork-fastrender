/// Pure policy helpers for deciding how the windowed UI should react when a download starts.
///
/// The windowed browser opens the downloads side panel automatically on
/// `WorkerToUi::DownloadStarted` so downloads triggered from page content are discoverable.
///
/// This logic is kept egui/winit-free so it can be unit tested without creating a real window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DownloadsPanelPolicyInput {
  /// True when the window receiving the worker message is the active window in the browser.
  ///
  /// Only the active window should auto-open its downloads panel; background windows should not
  /// steal space/focus.
  pub window_is_active: bool,
  /// True when the user is currently typing into a chrome text input (address bar, tab search,
  /// history search, etc.).
  ///
  /// When this is true, the policy should not request focus for the downloads panel.
  pub chrome_has_text_focus: bool,
  pub history_panel_open: bool,
  pub bookmarks_panel_open: bool,
  pub downloads_panel_open: bool,
  pub downloads_panel_request_focus: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DownloadsPanelPolicyOutput {
  pub history_panel_open: bool,
  pub bookmarks_panel_open: bool,
  pub downloads_panel_open: bool,
  pub downloads_panel_request_focus: bool,
}

impl DownloadsPanelPolicyOutput {
  fn same_as_input(input: DownloadsPanelPolicyInput) -> Self {
    Self {
      history_panel_open: input.history_panel_open,
      bookmarks_panel_open: input.bookmarks_panel_open,
      downloads_panel_open: input.downloads_panel_open,
      downloads_panel_request_focus: input.downloads_panel_request_focus,
    }
  }
}

/// Apply the downloads-panel auto-open policy for a `WorkerToUi::DownloadStarted` event.
pub fn on_download_started(input: DownloadsPanelPolicyInput) -> DownloadsPanelPolicyOutput {
  if !input.window_is_active {
    return DownloadsPanelPolicyOutput::same_as_input(input);
  }

  let mut out = DownloadsPanelPolicyOutput::same_as_input(input);

  // Ensure the downloads panel is visible.
  out.downloads_panel_open = true;

  // Focus rules:
  // - While the user is typing in a chrome text input, never request focus (avoid disrupting their
  //   typing, even if opening downloads closes another panel).
  // - Otherwise, request focus so keyboard users can immediately interact with downloads.
  if input.chrome_has_text_focus {
    out.downloads_panel_request_focus = false;
  } else {
    out.downloads_panel_request_focus = true;
  }

  // Keep the right-side panel area exclusive: downloads share the space with history/bookmarks.
  out.history_panel_open = false;
  out.bookmarks_panel_open = false;

  out
}

#[cfg(test)]
mod tests {
  use super::*;

  fn base_input() -> DownloadsPanelPolicyInput {
    DownloadsPanelPolicyInput {
      window_is_active: true,
      chrome_has_text_focus: false,
      history_panel_open: false,
      bookmarks_panel_open: false,
      downloads_panel_open: false,
      downloads_panel_request_focus: false,
    }
  }

  #[test]
  fn inactive_window_does_not_open_panel() {
    let mut input = base_input();
    input.window_is_active = false;
    input.history_panel_open = true;

    let out = on_download_started(input);
    assert_eq!(out, DownloadsPanelPolicyOutput::same_as_input(input));
  }

  #[test]
  fn opens_downloads_panel_and_closes_other_right_side_panels() {
    let mut input = base_input();
    input.history_panel_open = true;
    input.bookmarks_panel_open = true;

    let out = on_download_started(input);
    assert!(out.downloads_panel_open);
    assert!(!out.history_panel_open);
    assert!(!out.bookmarks_panel_open);
  }

  #[test]
  fn requests_focus_when_not_typing_in_chrome_text_input() {
    let input = base_input();
    let out = on_download_started(input);
    assert!(out.downloads_panel_open);
    assert!(out.downloads_panel_request_focus);
  }

  #[test]
  fn does_not_request_focus_when_typing_in_chrome_text_input() {
    let mut input = base_input();
    input.chrome_has_text_focus = true;

    let out = on_download_started(input);
    assert!(out.downloads_panel_open);
    assert!(!out.downloads_panel_request_focus);
  }

  #[test]
  fn requests_focus_when_panel_already_open_and_not_typing() {
    let mut input = base_input();
    input.downloads_panel_open = true;
    input.downloads_panel_request_focus = false;

    let out = on_download_started(input);
    assert!(out.downloads_panel_open);
    assert!(out.downloads_panel_request_focus);
  }

  #[test]
  fn typing_clears_existing_focus_request() {
    let mut input = base_input();
    input.chrome_has_text_focus = true;
    input.downloads_panel_open = true;
    input.downloads_panel_request_focus = true;

    let out = on_download_started(input);
    assert!(out.downloads_panel_open);
    assert!(!out.downloads_panel_request_focus);
  }
}
