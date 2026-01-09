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
  /// Activate a tab by its 1-based index (9 = last tab), matching typical browser shortcuts.
  ActivateTabNumber(u8),
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
  K,
  T,
  W,
  Tab,
  Left,
  Right,
  R,
  F5,
  Num1,
  Num2,
  Num3,
  Num4,
  Num5,
  Num6,
  Num7,
  Num8,
  Num9,
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
    (Key::K, Modifiers { ctrl: true, alt: false, .. }) => Some(ShortcutAction::FocusAddressBar),
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
    // Ctrl+1..9 switches tabs (9 = last tab).
    (Key::Num1, Modifiers { ctrl: true, alt: false, .. }) => Some(ShortcutAction::ActivateTabNumber(1)),
    (Key::Num2, Modifiers { ctrl: true, alt: false, .. }) => Some(ShortcutAction::ActivateTabNumber(2)),
    (Key::Num3, Modifiers { ctrl: true, alt: false, .. }) => Some(ShortcutAction::ActivateTabNumber(3)),
    (Key::Num4, Modifiers { ctrl: true, alt: false, .. }) => Some(ShortcutAction::ActivateTabNumber(4)),
    (Key::Num5, Modifiers { ctrl: true, alt: false, .. }) => Some(ShortcutAction::ActivateTabNumber(5)),
    (Key::Num6, Modifiers { ctrl: true, alt: false, .. }) => Some(ShortcutAction::ActivateTabNumber(6)),
    (Key::Num7, Modifiers { ctrl: true, alt: false, .. }) => Some(ShortcutAction::ActivateTabNumber(7)),
    (Key::Num8, Modifiers { ctrl: true, alt: false, .. }) => Some(ShortcutAction::ActivateTabNumber(8)),
    (Key::Num9, Modifiers { ctrl: true, alt: false, .. }) => Some(ShortcutAction::ActivateTabNumber(9)),
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
  fn ctrl_k_focuses_address_bar() {
    assert_eq!(
      map_shortcut(Key::K, Modifiers::new(true, false, false)),
      Some(ShortcutAction::FocusAddressBar)
    );
  }

  #[test]
  fn ctrl_1_selects_first_tab() {
    assert_eq!(
      map_shortcut(Key::Num1, Modifiers::new(true, false, false)),
      Some(ShortcutAction::ActivateTabNumber(1))
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
