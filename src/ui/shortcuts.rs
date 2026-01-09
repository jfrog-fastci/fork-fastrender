//! Keyboard shortcut mapping for the browser UI.
//!
//! This module is deliberately UI-framework agnostic so we can unit test it without needing a
//! windowing backend. The `browser` binary converts winit events into these simplified types.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShortcutAction {
  /// Focus the address bar and select all contents.
  FocusAddressBar,
  NewTab,
  CloseTab,
  NextTab,
  PrevTab,
  Back,
  Forward,
  Reload,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Modifiers {
  pub ctrl: bool,
  pub shift: bool,
  pub alt: bool,
}

impl Modifiers {
  pub const fn new(ctrl: bool, shift: bool, alt: bool) -> Self {
    Self { ctrl, shift, alt }
  }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Key {
  L,
  T,
  W,
  Tab,
  Left,
  Right,
  R,
  F5,
}

/// Map a simplified key event to a browser action.
///
/// The mapping follows common cross-platform browser defaults.
///
/// Note: We intentionally ignore combinations that include `alt` alongside `ctrl` to avoid
/// interfering with layouts that expose AltGr as Ctrl+Alt.
pub fn map_shortcut(key: Key, modifiers: Modifiers) -> Option<ShortcutAction> {
  match (key, modifiers) {
    (Key::L, Modifiers { ctrl: true, alt: false, .. }) => Some(ShortcutAction::FocusAddressBar),
    (Key::T, Modifiers { ctrl: true, alt: false, .. }) => Some(ShortcutAction::NewTab),
    (Key::W, Modifiers { ctrl: true, alt: false, .. }) => Some(ShortcutAction::CloseTab),
    (Key::Tab, Modifiers { ctrl: true, shift: false, alt: false }) => {
      Some(ShortcutAction::NextTab)
    }
    (Key::Tab, Modifiers { ctrl: true, shift: true, alt: false }) => {
      Some(ShortcutAction::PrevTab)
    }
    (Key::Left, Modifiers { alt: true, ctrl: false, .. }) => Some(ShortcutAction::Back),
    (Key::Right, Modifiers { alt: true, ctrl: false, .. }) => Some(ShortcutAction::Forward),
    (Key::R, Modifiers { ctrl: true, alt: false, .. }) => Some(ShortcutAction::Reload),
    // F5 should reload even without modifiers. Ignore Ctrl+F5 / Alt+F5 for now.
    (Key::F5, Modifiers { ctrl: false, alt: false, .. }) => Some(ShortcutAction::Reload),
    _ => None,
  }
}

#[cfg(test)]
mod tests {
  use super::{map_shortcut, Key, Modifiers, ShortcutAction};

  #[test]
  fn ctrl_l_focuses_address_bar() {
    assert_eq!(
      map_shortcut(Key::L, Modifiers::new(true, false, false)),
      Some(ShortcutAction::FocusAddressBar)
    );
  }

  #[test]
  fn ctrl_t_new_tab() {
    assert_eq!(
      map_shortcut(Key::T, Modifiers::new(true, false, false)),
      Some(ShortcutAction::NewTab)
    );
  }

  #[test]
  fn ctrl_w_close_tab() {
    assert_eq!(
      map_shortcut(Key::W, Modifiers::new(true, false, false)),
      Some(ShortcutAction::CloseTab)
    );
  }

  #[test]
  fn ctrl_tab_cycles_tabs() {
    assert_eq!(
      map_shortcut(Key::Tab, Modifiers::new(true, false, false)),
      Some(ShortcutAction::NextTab)
    );
    assert_eq!(
      map_shortcut(Key::Tab, Modifiers::new(true, true, false)),
      Some(ShortcutAction::PrevTab)
    );
  }

  #[test]
  fn alt_left_right_navigate_history() {
    assert_eq!(
      map_shortcut(Key::Left, Modifiers::new(false, false, true)),
      Some(ShortcutAction::Back)
    );
    assert_eq!(
      map_shortcut(Key::Right, Modifiers::new(false, false, true)),
      Some(ShortcutAction::Forward)
    );
  }

  #[test]
  fn reload_shortcuts() {
    assert_eq!(
      map_shortcut(Key::R, Modifiers::new(true, false, false)),
      Some(ShortcutAction::Reload)
    );
    assert_eq!(
      map_shortcut(Key::F5, Modifiers::new(false, false, false)),
      Some(ShortcutAction::Reload)
    );
  }

  #[test]
  fn ctrl_alt_is_not_treated_as_ctrl_shortcut() {
    // Guard against AltGr (often encoded as Ctrl+Alt).
    assert_eq!(
      map_shortcut(Key::T, Modifiers::new(true, false, true)),
      None
    );
  }
}

