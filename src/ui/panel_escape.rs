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
  use super::{downloads_panel_should_close_on_escape, history_panel_escape_action, PanelEscapeAction};

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
  fn downloads_panel_escape_guard_blocks_global_chrome_inputs() {
    assert!(!downloads_panel_should_close_on_escape(true, false, false));
    assert!(!downloads_panel_should_close_on_escape(false, true, false));
    assert!(!downloads_panel_should_close_on_escape(false, false, true));
  }

  #[test]
  fn downloads_panel_escape_guard_allows_close_when_no_global_focus() {
    assert!(downloads_panel_should_close_on_escape(false, false, false));
  }
}
