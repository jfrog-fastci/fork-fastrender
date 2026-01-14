//! Pure helpers for deciding how panels should respond to Escape.
//!
//! The windowed browser UI has several transient surfaces (side panels, overlays, etc) that all
//! want to listen for the Escape key. In many cases, Escape should be routed to a currently-focused
//! text input first (e.g. "Escape clears search") before falling back to closing the panel.
//!
//! This module is kept free of `egui` types so it can be unit tested without pulling in the full
//! windowing/UI stack.

/// Action to take when Escape is pressed for a particular surface.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PanelEscapeAction {
  /// Clear the search query but keep the panel open.
  ClearSearch,
  /// Close the panel.
  ClosePanel,
  /// Do nothing (let another part of chrome handle Escape).
  Noop,
}

/// Pure boolean state describing whether Escape should map to "Stop loading".
///
/// Escape has multiple meanings in the windowed browser UI:
/// - dismiss transient chrome UI (side panels, dialogs, menus, in-page popups),
/// - cancel find-in-page,
/// - otherwise stop an in-flight navigation.
///
/// This struct is deliberately egui-agnostic and can be unit tested without pulling in egui/winit.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct StopLoadingOnEscapeState {
  /// Whether a chrome text input owns keyboard focus (address bar, find-in-page input, panel search).
  pub chrome_has_text_focus: bool,
  /// Whether the active tab is currently loading.
  pub tab_loading: bool,
  /// Whether find-in-page is currently open.
  pub find_in_page_open: bool,
  /// Downloads panel is open.
  pub downloads_panel_open: bool,
  /// History panel is open.
  pub history_panel_open: bool,
  /// Bookmarks manager/panel is open.
  pub bookmarks_panel_open: bool,
  /// Clear browsing data modal dialog is open.
  pub clear_browsing_data_dialog_open: bool,
  /// Tab search / quick switcher overlay is open.
  pub tab_search_open: bool,
  /// A chrome menu (tab context menu, appearance menu, etc) is open.
  pub chrome_menu_open: bool,
  /// A page-scoped context menu is open (right-click menu).
  pub page_context_menu_open: bool,
  /// A page-scoped `<select>` dropdown popup is open.
  pub select_dropdown_open: bool,
  /// Media controls overlay is open.
  pub media_controls_open: bool,
  /// Other transient popups (date/time picker, color picker, file picker, …).
  pub other_popup_open: bool,
}

/// Decide whether Escape should trigger "Stop loading".
///
/// Returns `true` only when:
/// - the active tab is loading,
/// - egui/chrome is not actively editing text,
/// - no transient chrome/popup surface that should close on Escape is open.
#[must_use]
pub fn should_stop_loading_on_escape(state: StopLoadingOnEscapeState) -> bool {
  if state.chrome_has_text_focus {
    return false;
  }
  if !state.tab_loading {
    return false;
  }
  if state.find_in_page_open {
    return false;
  }
  if state.downloads_panel_open
    || state.history_panel_open
    || state.bookmarks_panel_open
    || state.clear_browsing_data_dialog_open
    || state.tab_search_open
    || state.chrome_menu_open
    || state.page_context_menu_open
    || state.select_dropdown_open
    || state.media_controls_open
    || state.other_popup_open
  {
    return false;
  }
  true
}

/// Decide how the Bookmarks Manager side panel should respond to Escape.
///
/// Semantics:
/// - If another chrome text surface is active (address bar, tab search, find-in-page), do not
///   handle Escape here (return [`PanelEscapeAction::Noop`]).
/// - Otherwise, if the bookmarks search query is non-empty and egui is in a keyboard-input state,
///   clear the search query (return [`PanelEscapeAction::ClearSearch`]).
/// - Otherwise close the panel (return [`PanelEscapeAction::ClosePanel`]).
#[must_use]
pub fn bookmarks_panel_escape_action(
  ctx_wants_keyboard_input: bool,
  bookmarks_search_is_empty: bool,
  address_bar_has_focus: bool,
  tab_search_open: bool,
  find_in_page_open: bool,
) -> PanelEscapeAction {
  if address_bar_has_focus || tab_search_open || find_in_page_open {
    return PanelEscapeAction::Noop;
  }

  if ctx_wants_keyboard_input && !bookmarks_search_is_empty {
    PanelEscapeAction::ClearSearch
  } else {
    PanelEscapeAction::ClosePanel
  }
}

/// Decide how the History side panel should respond to Escape.
///
/// Semantics:
/// - If another chrome text surface is active (address bar, tab search, find-in-page), do not
///   handle Escape here (return [`PanelEscapeAction::Noop`]).
/// - Otherwise, if the history search query is non-empty and egui is in a keyboard-input state,
///   clear the search query (return [`PanelEscapeAction::ClearSearch`]).
/// - Otherwise close the panel (return [`PanelEscapeAction::ClosePanel`]).
#[must_use]
pub fn history_panel_escape_action(
  ctx_wants_keyboard_input: bool,
  history_search_is_empty: bool,
  address_bar_has_focus: bool,
  tab_search_open: bool,
  find_in_page_open: bool,
) -> PanelEscapeAction {
  if address_bar_has_focus || tab_search_open || find_in_page_open {
    return PanelEscapeAction::Noop;
  }

  if ctx_wants_keyboard_input && !history_search_is_empty {
    PanelEscapeAction::ClearSearch
  } else {
    PanelEscapeAction::ClosePanel
  }
}

/// Decide whether the Downloads side panel should close when Escape is pressed.
///
/// The Downloads panel contains its own search `TextEdit`. When that input consumes Escape to clear
/// the query, we should *not* also close the panel. Therefore, callers should:
/// 1) run the panel UI first (so the search field can consume Escape when clearing), then
/// 2) if Escape is still available, use this helper to decide if the panel should close.
///
/// Semantics:
/// - If another chrome text surface is active (address bar, tab search, find-in-page), do not close
///   the Downloads panel here (return `false`).
/// - Otherwise, allow Escape to close the panel (return `true`).
#[must_use]
pub fn downloads_panel_should_close_on_escape(
  address_bar_has_focus: bool,
  tab_search_open: bool,
  find_in_page_open: bool,
) -> bool {
  !(address_bar_has_focus || tab_search_open || find_in_page_open)
}

#[cfg(test)]
mod tests {
  use super::{
    bookmarks_panel_escape_action, downloads_panel_should_close_on_escape,
    history_panel_escape_action, should_stop_loading_on_escape, PanelEscapeAction,
    StopLoadingOnEscapeState,
  };

  #[test]
  fn non_empty_search_clears() {
    assert_eq!(
      history_panel_escape_action(true, false, false, false, false),
      PanelEscapeAction::ClearSearch
    );
  }

  #[test]
  fn empty_search_closes_panel() {
    assert_eq!(
      history_panel_escape_action(true, true, false, false, false),
      PanelEscapeAction::ClosePanel
    );
  }

  #[test]
  fn address_bar_focus_is_noop() {
    assert_eq!(
      history_panel_escape_action(true, false, true, false, false),
      PanelEscapeAction::Noop
    );
  }

  #[test]
  fn tab_search_open_is_noop() {
    assert_eq!(
      history_panel_escape_action(true, false, false, true, false),
      PanelEscapeAction::Noop
    );
  }

  #[test]
  fn find_in_page_open_is_noop() {
    assert_eq!(
      history_panel_escape_action(true, false, false, false, true),
      PanelEscapeAction::Noop
    );
  }

  #[test]
  fn bookmarks_non_empty_search_clears() {
    assert_eq!(
      bookmarks_panel_escape_action(true, false, false, false, false),
      PanelEscapeAction::ClearSearch
    );
  }

  #[test]
  fn bookmarks_empty_search_closes_panel() {
    assert_eq!(
      bookmarks_panel_escape_action(true, true, false, false, false),
      PanelEscapeAction::ClosePanel
    );
  }

  #[test]
  fn bookmarks_address_bar_focus_is_noop() {
    assert_eq!(
      bookmarks_panel_escape_action(true, false, true, false, false),
      PanelEscapeAction::Noop
    );
  }

  #[test]
  fn bookmarks_tab_search_open_is_noop() {
    assert_eq!(
      bookmarks_panel_escape_action(true, false, false, true, false),
      PanelEscapeAction::Noop
    );
  }

  #[test]
  fn bookmarks_find_open_is_noop() {
    assert_eq!(
      bookmarks_panel_escape_action(true, false, false, false, true),
      PanelEscapeAction::Noop
    );
  }

  #[test]
  fn downloads_panel_escape_guard_blocks_global_chrome_inputs() {
    assert!(!downloads_panel_should_close_on_escape(true, false, false));
    assert!(!downloads_panel_should_close_on_escape(false, true, false));
    assert!(!downloads_panel_should_close_on_escape(false, false, true));
  }

  #[test]
  fn downloads_panel_escape_guard_allows_close_when_no_global_focus() {
    assert!(downloads_panel_should_close_on_escape(false, false, false));
  }

  #[test]
  fn stop_loading_blocked_when_downloads_panel_open() {
    assert!(!should_stop_loading_on_escape(StopLoadingOnEscapeState {
      tab_loading: true,
      downloads_panel_open: true,
      ..Default::default()
    }));
  }

  #[test]
  fn stop_loading_blocked_when_history_or_bookmarks_panel_open() {
    assert!(!should_stop_loading_on_escape(StopLoadingOnEscapeState {
      tab_loading: true,
      history_panel_open: true,
      ..Default::default()
    }));
    assert!(!should_stop_loading_on_escape(StopLoadingOnEscapeState {
      tab_loading: true,
      bookmarks_panel_open: true,
      ..Default::default()
    }));
  }

  #[test]
  fn stop_loading_blocked_when_find_in_page_open() {
    assert!(!should_stop_loading_on_escape(StopLoadingOnEscapeState {
      tab_loading: true,
      find_in_page_open: true,
      ..Default::default()
    }));
  }

  #[test]
  fn stop_loading_allowed_when_tab_loading_and_no_chrome_surfaces_open() {
    assert!(should_stop_loading_on_escape(StopLoadingOnEscapeState {
      tab_loading: true,
      ..Default::default()
    }));
  }
}
