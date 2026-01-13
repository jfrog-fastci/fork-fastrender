//! Toolkit-agnostic omnibox keyboard navigation helpers.
//!
//! The egui chrome UI (`src/ui/chrome.rs`) and the renderer-driven chrome frame both need to share
//! the same selection / accept / dismiss behaviour for the omnibox dropdown.

use crate::ui::browser_app::BrowserAppState;
use crate::ui::chrome_action::ChromeAction;
use crate::ui::omnibox::{OmniboxAction, OmniboxSuggestion};
use crate::ui::url::{search_url_for_query, DEFAULT_SEARCH_ENGINE_TEMPLATE};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OmniboxNavKey {
  ArrowUp,
  ArrowDown,
  Enter,
  Escape,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OmniboxNavOutcome {
  /// Action accepted by Enter, if any.
  pub action: Option<ChromeAction>,
  /// Whether the omnibox dropdown should remain open after handling the key.
  pub keep_dropdown_open: bool,
}

pub fn omnibox_suggestion_fill_text(suggestion: &OmniboxSuggestion) -> Option<&str> {
  match &suggestion.action {
    OmniboxAction::NavigateToUrl => suggestion.url.as_deref(),
    OmniboxAction::ActivateTab(_) => suggestion.url.as_deref(),
    OmniboxAction::Search(query) => Some(query),
  }
}

pub fn omnibox_suggestion_accept_action(suggestion: &OmniboxSuggestion) -> ChromeAction {
  match &suggestion.action {
    OmniboxAction::NavigateToUrl => {
      ChromeAction::NavigateTo(suggestion.url.clone().unwrap_or_default())
    }
    OmniboxAction::ActivateTab(tab_id) => ChromeAction::ActivateTab(*tab_id),
    OmniboxAction::Search(query) => ChromeAction::NavigateTo(
      search_url_for_query(query, DEFAULT_SEARCH_ENGINE_TEMPLATE).unwrap_or_else(|_| query.clone()),
    ),
  }
}

/// Apply a keyboard navigation key (ArrowUp/Down/Enter/Escape) to the omnibox UI state.
///
/// This function mutates `app.chrome` in-place and returns a high-level outcome so UI front-ends can
/// emit actions and update their own focus/DOM state.
pub fn apply_omnibox_nav_key(app: &mut BrowserAppState, key: OmniboxNavKey) -> OmniboxNavOutcome {
  match key {
    OmniboxNavKey::ArrowDown | OmniboxNavKey::ArrowUp => {
      let len = app.chrome.omnibox.suggestions.len();
      if len == 0 {
        // Nothing to navigate.
        app.chrome.omnibox.open = false;
        app.chrome.omnibox.selected = None;
        return OmniboxNavOutcome {
          action: None,
          keep_dropdown_open: false,
        };
      }

      app.chrome.omnibox.open = true;
      let selected = app.chrome.omnibox.selected.filter(|i| *i < len);
      let next = match key {
        OmniboxNavKey::ArrowDown => match selected {
          None => 0,
          Some(i) => (i + 1) % len,
        },
        OmniboxNavKey::ArrowUp => match selected {
          None => len - 1,
          Some(i) => (i + len - 1) % len,
        },
        _ => unreachable!(), // fastrender-allow-panic
      };

      if app.chrome.omnibox.selected.is_none() && app.chrome.omnibox.original_input.is_none() {
        app.chrome.omnibox.original_input = Some(app.chrome.address_bar_text.clone());
      }
      app.chrome.omnibox.selected = Some(next);

      if let Some(suggestion) = app.chrome.omnibox.suggestions.get(next) {
        if let Some(fill) = omnibox_suggestion_fill_text(suggestion) {
          app.chrome.address_bar_text = fill.to_string();
        }
      }

      OmniboxNavOutcome {
        action: None,
        keep_dropdown_open: app.chrome.omnibox.open,
      }
    }
    OmniboxNavKey::Escape => {
      if app.chrome.omnibox.open || app.chrome.omnibox.selected.is_some() {
        app.chrome.omnibox.open = false;
        app.chrome.omnibox.selected = None;
        if let Some(original) = app.chrome.omnibox.original_input.take() {
          app.chrome.address_bar_text = original;
        }
      } else {
        // Match browser UX: Escape dismisses the address bar when no dropdown is open.
        app.set_address_bar_editing(false);
      }

      OmniboxNavOutcome {
        action: None,
        keep_dropdown_open: app.chrome.omnibox.open,
      }
    }
    OmniboxNavKey::Enter => {
      let accept_action = app
        .chrome
        .omnibox
        .open
        .then_some(())
        .and_then(|_| app.chrome.omnibox.selected)
        .and_then(|i| app.chrome.omnibox.suggestions.get(i))
        .map(omnibox_suggestion_accept_action);

      // First resolve the omnibox selection the same way plain Enter does.
      let resolved_action =
        accept_action.unwrap_or_else(|| ChromeAction::NavigateTo(app.chrome.address_bar_text.clone()));

      // Keep the address bar text consistent with what we just accepted when it represents a URL.
      if let ChromeAction::NavigateTo(url) = &resolved_action {
        app.chrome.address_bar_text = url.clone();
      }

      app.chrome.address_bar_editing = false;
      app.chrome.address_bar_has_focus = false;
      app.chrome.omnibox.reset();

      OmniboxNavOutcome {
        action: Some(resolved_action),
        keep_dropdown_open: false,
      }
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::ui::omnibox::{OmniboxSuggestionSource, OmniboxUrlSource};

  fn suggestion_navigate(url: &str) -> OmniboxSuggestion {
    OmniboxSuggestion {
      action: OmniboxAction::NavigateToUrl,
      title: None,
      url: Some(url.to_string()),
      source: OmniboxSuggestionSource::Url(OmniboxUrlSource::Visited),
    }
  }

  #[test]
  fn arrow_navigation_restores_original_input_on_escape() {
    let mut app = BrowserAppState::new_with_initial_tab("https://example.com/".to_string());
    app.chrome.address_bar_text = "typed input".to_string();
    app.chrome.address_bar_editing = true;
    app.chrome.address_bar_has_focus = true;

    app.chrome.omnibox.suggestions = vec![
      suggestion_navigate("https://a.example/"),
      suggestion_navigate("https://b.example/"),
    ];
    assert!(!app.chrome.omnibox.open);
    assert_eq!(app.chrome.omnibox.selected, None);

    apply_omnibox_nav_key(&mut app, OmniboxNavKey::ArrowDown);
    assert!(app.chrome.omnibox.open);
    assert_eq!(app.chrome.omnibox.selected, Some(0));
    assert_eq!(app.chrome.address_bar_text, "https://a.example/");
    assert_eq!(app.chrome.omnibox.original_input.as_deref(), Some("typed input"));

    apply_omnibox_nav_key(&mut app, OmniboxNavKey::ArrowDown);
    assert_eq!(app.chrome.omnibox.selected, Some(1));
    assert_eq!(app.chrome.address_bar_text, "https://b.example/");

    apply_omnibox_nav_key(&mut app, OmniboxNavKey::Escape);
    assert!(!app.chrome.omnibox.open);
    assert_eq!(app.chrome.omnibox.selected, None);
    assert_eq!(app.chrome.address_bar_text, "typed input");
    assert_eq!(app.chrome.omnibox.original_input, None);
  }

  #[test]
  fn enter_accepts_selected_suggestion() {
    let mut app = BrowserAppState::new_with_initial_tab("https://example.com/".to_string());
    app.chrome.address_bar_text = "typed input".to_string();
    app.chrome.address_bar_editing = true;
    app.chrome.address_bar_has_focus = true;
    app.chrome.omnibox.open = true;
    app.chrome.omnibox.selected = Some(0);
    app.chrome.omnibox.suggestions = vec![suggestion_navigate("https://accepted.example/")];

    let outcome = apply_omnibox_nav_key(&mut app, OmniboxNavKey::Enter);
    assert_eq!(
      outcome.action,
      Some(ChromeAction::NavigateTo("https://accepted.example/".to_string()))
    );
    assert!(!app.chrome.address_bar_editing);
    assert!(!app.chrome.address_bar_has_focus);
    assert!(!app.chrome.omnibox.open);
  }
}
