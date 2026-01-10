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
  ReopenClosedTab,
  NextTab,
  PrevTab,
  Back,
  Forward,
  Reload,
  /// Activate a tab by its 1-based index (9 = last tab), matching typical browser shortcuts.
  ActivateTabNumber(u8),
  ZoomIn,
  ZoomOut,
  ZoomReset,
  Copy,
  Cut,
  Paste,
  SelectAll,
  PageUp,
  PageDown,
  Space,
  Home,
  End,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Modifiers {
  pub ctrl: bool,
  pub shift: bool,
  pub alt: bool,
  /// "Meta" modifier: Command on macOS, Windows/Super key elsewhere.
  pub meta: bool,
}

impl Modifiers {
  pub const fn new(ctrl: bool, shift: bool, alt: bool, meta: bool) -> Self {
    Self {
      ctrl,
      shift,
      alt,
      meta,
    }
  }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Key {
  A,
  C,
  K,
  L,
  Minus,
  Num0,
  T,
  R,
  V,
  W,
  X,
  Tab,
  Left,
  Right,
  Plus,
  Equals,
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
  PageUp,
  PageDown,
  Space,
  Home,
  End,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeyEvent {
  pub key: Key,
  pub modifiers: Modifiers,
}

impl KeyEvent {
  pub const fn new(key: Key, modifiers: Modifiers) -> Self {
    Self { key, modifiers }
  }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Platform {
  Mac,
  Other,
}

impl Platform {
  pub const fn current() -> Self {
    if cfg!(target_os = "macos") {
      Platform::Mac
    } else {
      Platform::Other
    }
  }
}

/// Map a simplified key event to a browser action.
///
/// The mapping follows common cross-platform browser defaults.
///
/// Note: We intentionally ignore combinations that include `alt` alongside `ctrl` to avoid
/// interfering with layouts that expose AltGr as Ctrl+Alt.
pub fn map_shortcut(event: KeyEvent) -> Option<ShortcutAction> {
  map_shortcut_with_platform(event, Platform::current())
}

pub fn map_shortcut_with_platform(event: KeyEvent, platform: Platform) -> Option<ShortcutAction> {
  let KeyEvent { key, modifiers } = event;

  // Chrome-style "command" modifier: Ctrl on most platforms, Command on macOS.
  //
  // We intentionally ignore Ctrl+Alt combinations to avoid treating AltGr input as browser
  // shortcuts.
  let cmd = {
    if modifiers.alt {
      false
    } else {
      match platform {
        Platform::Mac => modifiers.meta || modifiers.ctrl,
        Platform::Other => modifiers.ctrl,
      }
    }
  };

  match (key, modifiers) {
    // Many browsers support both Ctrl/Cmd+L and Ctrl/Cmd+K for focusing the address bar.
    (Key::L | Key::K, _) if cmd => Some(ShortcutAction::FocusAddressBar),

    // Tabs.
    (Key::T, Modifiers { shift: true, .. }) if cmd => Some(ShortcutAction::ReopenClosedTab),
    (Key::T, _) if cmd => Some(ShortcutAction::NewTab),
    (Key::W, _) if cmd => Some(ShortcutAction::CloseTab),
    (Key::Tab, Modifiers { shift: true, .. }) if cmd => Some(ShortcutAction::PrevTab),
    (Key::Tab, _) if cmd => Some(ShortcutAction::NextTab),

    // Navigation.
    (Key::Left, Modifiers { alt: true, ctrl: false, meta: false, .. }) => {
      Some(ShortcutAction::Back)
    }
    (Key::Right, Modifiers { alt: true, ctrl: false, meta: false, .. }) => {
      Some(ShortcutAction::Forward)
    }
    (Key::R, _) if cmd => Some(ShortcutAction::Reload),
    // F5 should reload even without modifiers. Ignore Ctrl/Cmd+F5 / Alt+F5 for now.
    (Key::F5, Modifiers { alt: false, ctrl: false, meta: false, .. }) => Some(ShortcutAction::Reload),

    // Ctrl/Cmd+1..9 switches tabs (9 = last tab).
    (Key::Num1, _) if cmd => Some(ShortcutAction::ActivateTabNumber(1)),
    (Key::Num2, _) if cmd => Some(ShortcutAction::ActivateTabNumber(2)),
    (Key::Num3, _) if cmd => Some(ShortcutAction::ActivateTabNumber(3)),
    (Key::Num4, _) if cmd => Some(ShortcutAction::ActivateTabNumber(4)),
    (Key::Num5, _) if cmd => Some(ShortcutAction::ActivateTabNumber(5)),
    (Key::Num6, _) if cmd => Some(ShortcutAction::ActivateTabNumber(6)),
    (Key::Num7, _) if cmd => Some(ShortcutAction::ActivateTabNumber(7)),
    (Key::Num8, _) if cmd => Some(ShortcutAction::ActivateTabNumber(8)),
    (Key::Num9, _) if cmd => Some(ShortcutAction::ActivateTabNumber(9)),

    // Zoom.
    (Key::Plus | Key::Equals, _) if cmd => Some(ShortcutAction::ZoomIn),
    (Key::Minus, _) if cmd => Some(ShortcutAction::ZoomOut),
    (Key::Num0, _) if cmd => Some(ShortcutAction::ZoomReset),

    // Clipboard.
    (Key::C, Modifiers { shift: false, .. }) if cmd => Some(ShortcutAction::Copy),
    (Key::X, Modifiers { shift: false, .. }) if cmd => Some(ShortcutAction::Cut),
    (Key::V, Modifiers { shift: false, .. }) if cmd => Some(ShortcutAction::Paste),
    (Key::A, Modifiers { shift: false, .. }) if cmd => Some(ShortcutAction::SelectAll),

    // Scrolling / page keys.
    (Key::PageUp, Modifiers { ctrl: false, shift: false, alt: false, meta: false }) => {
      Some(ShortcutAction::PageUp)
    }
    (Key::PageDown, Modifiers { ctrl: false, shift: false, alt: false, meta: false }) => {
      Some(ShortcutAction::PageDown)
    }
    // Match `Space` regardless of `shift` so the caller can implement `Shift+Space` scrolling up
    // (common browser behaviour).
    (Key::Space, Modifiers { ctrl: false, alt: false, meta: false, .. }) => Some(ShortcutAction::Space),
    (Key::Home, Modifiers { ctrl: false, shift: false, alt: false, meta: false }) => {
      Some(ShortcutAction::Home)
    }
    (Key::End, Modifiers { ctrl: false, shift: false, alt: false, meta: false }) => {
      Some(ShortcutAction::End)
    }

    _ => None,
  }
}

#[cfg(test)]
mod tests {
  use super::{map_shortcut_with_platform, Key, KeyEvent, Modifiers, Platform, ShortcutAction};

  #[test]
  fn ctrl_l_focuses_address_bar() {
    assert_eq!(
      map_shortcut_with_platform(
        KeyEvent::new(Key::L, Modifiers::new(true, false, false, false)),
        Platform::Other
      ),
      Some(ShortcutAction::FocusAddressBar)
    );
  }

  #[test]
  fn ctrl_k_focuses_address_bar() {
    assert_eq!(
      map_shortcut_with_platform(
        KeyEvent::new(Key::K, Modifiers::new(true, false, false, false)),
        Platform::Other
      ),
      Some(ShortcutAction::FocusAddressBar)
    );
  }

  #[test]
  fn ctrl_1_selects_first_tab() {
    assert_eq!(
      map_shortcut_with_platform(
        KeyEvent::new(Key::Num1, Modifiers::new(true, false, false, false)),
        Platform::Other
      ),
      Some(ShortcutAction::ActivateTabNumber(1))
    );
  }

  #[test]
  fn ctrl_t_new_tab() {
    assert_eq!(
      map_shortcut_with_platform(
        KeyEvent::new(Key::T, Modifiers::new(true, false, false, false)),
        Platform::Other
      ),
      Some(ShortcutAction::NewTab)
    );
  }

  #[test]
  fn ctrl_shift_t_reopens_closed_tab() {
    assert_eq!(
      map_shortcut_with_platform(
        KeyEvent::new(Key::T, Modifiers::new(true, true, false, false)),
        Platform::Other
      ),
      Some(ShortcutAction::ReopenClosedTab)
    );
  }

  #[test]
  fn ctrl_w_close_tab() {
    assert_eq!(
      map_shortcut_with_platform(
        KeyEvent::new(Key::W, Modifiers::new(true, false, false, false)),
        Platform::Other
      ),
      Some(ShortcutAction::CloseTab)
    );
  }

  #[test]
  fn ctrl_tab_cycles_tabs() {
    assert_eq!(
      map_shortcut_with_platform(
        KeyEvent::new(Key::Tab, Modifiers::new(true, false, false, false)),
        Platform::Other
      ),
      Some(ShortcutAction::NextTab)
    );
    assert_eq!(
      map_shortcut_with_platform(
        KeyEvent::new(Key::Tab, Modifiers::new(true, true, false, false)),
        Platform::Other
      ),
      Some(ShortcutAction::PrevTab)
    );
  }

  #[test]
  fn alt_left_right_navigate_history() {
    assert_eq!(
      map_shortcut_with_platform(
        KeyEvent::new(Key::Left, Modifiers::new(false, false, true, false)),
        Platform::Other
      ),
      Some(ShortcutAction::Back)
    );
    assert_eq!(
      map_shortcut_with_platform(
        KeyEvent::new(Key::Right, Modifiers::new(false, false, true, false)),
        Platform::Other
      ),
      Some(ShortcutAction::Forward)
    );
  }

  #[test]
  fn reload_shortcuts() {
    assert_eq!(
      map_shortcut_with_platform(
        KeyEvent::new(Key::R, Modifiers::new(true, false, false, false)),
        Platform::Other
      ),
      Some(ShortcutAction::Reload)
    );
    assert_eq!(
      map_shortcut_with_platform(
        KeyEvent::new(Key::F5, Modifiers::new(false, false, false, false)),
        Platform::Other
      ),
      Some(ShortcutAction::Reload)
    );
    assert_eq!(
      map_shortcut_with_platform(
        KeyEvent::new(Key::F5, Modifiers::new(true, false, false, false)),
        Platform::Other
      ),
      None
    );
  }

  #[test]
  fn ctrl_alt_is_not_treated_as_ctrl_shortcut() {
    // Guard against AltGr (often encoded as Ctrl+Alt).
    assert_eq!(
      map_shortcut_with_platform(
        KeyEvent::new(Key::T, Modifiers::new(true, false, true, false)),
        Platform::Other
      ),
      None
    );
  }

  #[test]
  fn zoom_shortcuts() {
    assert_eq!(
      map_shortcut_with_platform(
        KeyEvent::new(Key::Equals, Modifiers::new(true, false, false, false)),
        Platform::Other
      ),
      Some(ShortcutAction::ZoomIn)
    );
    // `Ctrl+Shift+=` is the common way to produce `+` on US layouts; still zoom in.
    assert_eq!(
      map_shortcut_with_platform(
        KeyEvent::new(Key::Equals, Modifiers::new(true, true, false, false)),
        Platform::Other
      ),
      Some(ShortcutAction::ZoomIn)
    );
    assert_eq!(
      map_shortcut_with_platform(
        KeyEvent::new(Key::Minus, Modifiers::new(true, false, false, false)),
        Platform::Other
      ),
      Some(ShortcutAction::ZoomOut)
    );
    assert_eq!(
      map_shortcut_with_platform(
        KeyEvent::new(Key::Num0, Modifiers::new(true, false, false, false)),
        Platform::Other
      ),
      Some(ShortcutAction::ZoomReset)
    );
  }

  #[test]
  fn clipboard_shortcuts() {
    assert_eq!(
      map_shortcut_with_platform(
        KeyEvent::new(Key::C, Modifiers::new(true, false, false, false)),
        Platform::Other
      ),
      Some(ShortcutAction::Copy)
    );
    assert_eq!(
      map_shortcut_with_platform(
        KeyEvent::new(Key::X, Modifiers::new(true, false, false, false)),
        Platform::Other
      ),
      Some(ShortcutAction::Cut)
    );
    assert_eq!(
      map_shortcut_with_platform(
        KeyEvent::new(Key::V, Modifiers::new(true, false, false, false)),
        Platform::Other
      ),
      Some(ShortcutAction::Paste)
    );
    assert_eq!(
      map_shortcut_with_platform(
        KeyEvent::new(Key::A, Modifiers::new(true, false, false, false)),
        Platform::Other
      ),
      Some(ShortcutAction::SelectAll)
    );
    // Ensure we don't accidentally treat Ctrl+Shift+C (often used for devtools) as Copy.
    assert_eq!(
      map_shortcut_with_platform(
        KeyEvent::new(Key::C, Modifiers::new(true, true, false, false)),
        Platform::Other
      ),
      None
    );
    // AltGr should not trigger clipboard shortcuts.
    assert_eq!(
      map_shortcut_with_platform(
        KeyEvent::new(Key::C, Modifiers::new(true, false, true, false)),
        Platform::Other
      ),
      None
    );
  }

  #[test]
  fn page_scrolling_keys_are_mapped_without_modifiers() {
    for (key, expected) in [
      (Key::PageUp, ShortcutAction::PageUp),
      (Key::PageDown, ShortcutAction::PageDown),
      (Key::Space, ShortcutAction::Space),
      (Key::Home, ShortcutAction::Home),
      (Key::End, ShortcutAction::End),
    ] {
      assert_eq!(
        map_shortcut_with_platform(KeyEvent::new(key, Modifiers::default()), Platform::Other),
        Some(expected)
      );
      // Modifiers should generally suppress scroll actions so we don't interfere with selection
      // shortcuts (except Shift+Space).
      let shifted = map_shortcut_with_platform(
        KeyEvent::new(key, Modifiers::new(false, true, false, false)),
        Platform::Other,
      );
      if matches!(key, Key::Space) {
        assert_eq!(shifted, Some(ShortcutAction::Space));
      } else {
        assert_eq!(shifted, None);
      }
    }
  }

  #[test]
  fn mac_uses_meta_as_the_primary_modifier() {
    assert_eq!(
      map_shortcut_with_platform(
        KeyEvent::new(Key::T, Modifiers::new(false, false, false, true)),
        Platform::Mac
      ),
      Some(ShortcutAction::NewTab)
    );
    // On non-mac platforms, Meta is not the primary shortcut modifier.
    assert_eq!(
      map_shortcut_with_platform(
        KeyEvent::new(Key::T, Modifiers::new(false, false, false, true)),
        Platform::Other
      ),
      None
    );
  }
}
