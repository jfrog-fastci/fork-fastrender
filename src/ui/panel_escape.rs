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

#[cfg(test)]
mod tests {
  use super::{history_panel_escape_action, PanelEscapeAction};

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
}
