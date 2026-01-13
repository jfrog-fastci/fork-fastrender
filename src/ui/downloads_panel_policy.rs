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
  /// True when the address bar is considered focused (even if egui does not report a focused
  /// `TextEdit` yet).
  ///
  /// This mirrors the windowed browser's `chrome_has_text_focus` derivation, but is kept separate
  /// so we can conservatively avoid auto-open/focus during address bar focus transitions.
  pub address_bar_has_focus: bool,
  /// True when a modal dialog is open that should suppress non-essential UI changes.
  ///
  /// Today this is primarily the "Clear browsing data" dialog.
  pub clear_browsing_data_dialog_open: bool,
  /// True when any other popup/picker/menu is open (context menu, `<select>` dropdown, etc.).
  pub other_popup_open: bool,
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

/// Pure policy for whether we should auto-open the downloads panel for a newly started download.
///
/// This is intentionally conservative: if the user is typing in a chrome text input, or if a modal
/// is visible, we skip auto-opening (best-effort) to avoid disrupting their flow.
pub fn should_auto_open_downloads_panel(
  downloads_panel_open: bool,
  chrome_wants_keyboard_input: bool,
  address_bar_has_focus: bool,
  clear_browsing_data_dialog_open: bool,
  other_popup_open: bool,
) -> bool {
  !downloads_panel_open
    && !chrome_wants_keyboard_input
    && !address_bar_has_focus
    && !clear_browsing_data_dialog_open
    && !other_popup_open
}

/// Apply the downloads-panel auto-open policy for a `WorkerToUi::DownloadStarted` event.
pub fn on_download_started(input: DownloadsPanelPolicyInput) -> DownloadsPanelPolicyOutput {
  if !input.window_is_active {
    return DownloadsPanelPolicyOutput::same_as_input(input);
  }

  if !should_auto_open_downloads_panel(
    input.downloads_panel_open,
    input.chrome_has_text_focus,
    input.address_bar_has_focus,
    input.clear_browsing_data_dialog_open,
    input.other_popup_open,
  ) {
    return DownloadsPanelPolicyOutput::same_as_input(input);
  }

  let mut out = DownloadsPanelPolicyOutput::same_as_input(input);
  out.downloads_panel_open = true;
  out.downloads_panel_request_focus = true;
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
      address_bar_has_focus: false,
      clear_browsing_data_dialog_open: false,
      other_popup_open: false,
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
  fn requests_focus_when_idle() {
    let input = base_input();
    let out = on_download_started(input);
    assert!(out.downloads_panel_open);
    assert!(out.downloads_panel_request_focus);
  }

  #[test]
  fn does_not_open_when_typing_in_chrome_text_input() {
    let mut input = base_input();
    input.chrome_has_text_focus = true;

    let out = on_download_started(input);
    assert_eq!(out, DownloadsPanelPolicyOutput::same_as_input(input));
  }

  #[test]
  fn does_not_open_when_panel_already_open() {
    let mut input = base_input();
    input.downloads_panel_open = true;
    input.downloads_panel_request_focus = false;

    let out = on_download_started(input);
    assert_eq!(out, DownloadsPanelPolicyOutput::same_as_input(input));
  }

  #[test]
  fn does_not_open_when_clear_browsing_data_dialog_open() {
    let mut input = base_input();
    input.clear_browsing_data_dialog_open = true;

    let out = on_download_started(input);
    assert_eq!(out, DownloadsPanelPolicyOutput::same_as_input(input));
  }

  #[test]
  fn should_auto_open_downloads_panel_policy_matches_expected_conditions() {
    // Busy typing in a chrome input: do not auto-open.
    assert!(!should_auto_open_downloads_panel(
      false, true, false, false, false
    ));
    // Address bar focus transition: do not auto-open.
    assert!(!should_auto_open_downloads_panel(
      false, false, true, false, false
    ));
    // Modal open: do not auto-open.
    assert!(!should_auto_open_downloads_panel(
      false, false, false, true, false
    ));
    // Idle: ok to auto-open.
    assert!(should_auto_open_downloads_panel(
      false, false, false, false, false
    ));
  }
}
