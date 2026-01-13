//! Keyboard shortcut mapping for the browser UI.
//!
//! This module is deliberately UI-framework agnostic so we can unit test it without needing a
//! windowing backend. The `browser` binary converts winit events into these simplified types.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShortcutAction {
  /// Focus the address bar and select all contents.
  FocusAddressBar,
  FindInPage,
  /// Toggle the bookmarks manager UI surface.
  ToggleBookmarksManager,
  /// Toggle visibility of the downloads panel.
  ToggleDownloadsPanel,
  /// Open a new top-level browser window.
  NewWindow,
  /// Open the tab search / quick switcher overlay (Ctrl/Cmd+Shift+A).
  OpenTabSearch,
  NewTab,
  CloseTab,
  ReopenClosedTab,
  NextTab,
  PrevTab,
  Back,
  Forward,
  Reload,
  /// Navigate to the browser's configured home page.
  GoHome,
  /// Toggle bookmarking for the current page (Ctrl/Cmd+D).
  ToggleBookmark,
  /// Show the global browsing history UI surface (Ctrl+H / Cmd+Y).
  ShowHistory,
  /// Show the bookmarks manager UI surface (Ctrl/Cmd+Shift+O).
  ShowBookmarksManager,
  /// Toggle visibility of the bookmarks bar (Ctrl/Cmd+Shift+B).
  ToggleBookmarksBar,
  /// Open the "Clear browsing data" dialog (Ctrl/Cmd+Shift+Delete).
  OpenClearBrowsingDataDialog,
  /// Activate a tab by its 1-based index (9 = last tab), matching typical browser shortcuts.
  ActivateTabNumber(u8),
  ZoomIn,
  ZoomOut,
  ZoomReset,
  /// Toggle window fullscreen state.
  ToggleFullScreen,
  /// Save the current page (Ctrl/Cmd+S).
  ///
  /// Reserved so pages cannot intercept it even if the chrome UI does not implement saving.
  SavePage,
  /// Print the current page (Ctrl/Cmd+P).
  ///
  /// Reserved so pages cannot intercept it even if the chrome UI does not implement printing.
  PrintPage,
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
  B,
  C,
  D,
  F,
  H,
  J,
  K,
  L,
  N,
  O,
  P,
  OpenBracket,
  CloseBracket,
  Minus,
  Num0,
  S,
  T,
  R,
  V,
  W,
  X,
  Y,
  Insert,
  Delete,
  Tab,
  Left,
  Right,
  Plus,
  Equals,
  F4,
  F5,
  F6,
  F11,
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

  // "Primary" browser shortcut modifier: Ctrl on most platforms, Cmd on macOS.
  //
  // We intentionally do *not* treat macOS Ctrl as equivalent here because many macOS text controls
  // use Ctrl+<letter> as Emacs-style editing commands (e.g. Ctrl+D = forward delete).
  let primary_cmd = {
    if modifiers.alt {
      false
    } else {
      match platform {
        Platform::Mac => modifiers.meta,
        Platform::Other => modifiers.ctrl,
      }
    }
  };

  match (key, modifiers) {
    // Many browsers on Windows/Linux also support Alt+D for focusing the address bar.
    //
    // Avoid mapping this on macOS because the Option/Alt key participates in text entry.
    (
      Key::D,
      Modifiers {
        ctrl: false,
        shift: false,
        alt: true,
        meta: false,
      },
    ) if matches!(platform, Platform::Other) => Some(ShortcutAction::FocusAddressBar),
    // Many browsers support both Ctrl/Cmd+L and Ctrl/Cmd+K for focusing the address bar.
    (Key::L | Key::K, _) if cmd => Some(ShortcutAction::FocusAddressBar),
    // Many browsers support F6 to focus the address bar.
    (
      Key::F6,
      Modifiers {
        ctrl: false,
        shift: false,
        alt: false,
        meta: false,
      },
    ) => Some(ShortcutAction::FocusAddressBar),

    // Full screen.
    (
      Key::F11,
      Modifiers {
        ctrl: false,
        shift: false,
        alt: false,
        meta: false,
      },
    ) if matches!(platform, Platform::Other) => Some(ShortcutAction::ToggleFullScreen),
    (
      Key::F,
      Modifiers {
        ctrl: true,
        shift: false,
        alt: false,
        meta: true,
      },
    ) if matches!(platform, Platform::Mac) => Some(ShortcutAction::ToggleFullScreen),

    (Key::F, Modifiers { shift: false, .. }) if cmd => Some(ShortcutAction::FindInPage),

    // Tabs.
    (Key::N, Modifiers { shift: false, .. }) if cmd => Some(ShortcutAction::NewWindow),
    (Key::T, Modifiers { shift: true, .. }) if cmd => Some(ShortcutAction::ReopenClosedTab),
    (Key::T, _) if cmd => Some(ShortcutAction::NewTab),
    // Chrome/Chromium: Ctrl/Cmd+Shift+A opens "Search tabs" / tab switcher.
    (Key::A, Modifiers { shift: true, .. }) if cmd => Some(ShortcutAction::OpenTabSearch),
    (Key::W, _) if cmd => Some(ShortcutAction::CloseTab),
    (Key::F4, _) if cmd => Some(ShortcutAction::CloseTab),
    (Key::Tab, Modifiers { shift: true, .. }) if cmd => Some(ShortcutAction::PrevTab),
    (Key::Tab, _) if cmd => Some(ShortcutAction::NextTab),
    // Many browsers (notably Firefox/Chromium on Windows/Linux) also support Ctrl+PageUp/PageDown
    // for tab cycling.
    (Key::PageUp, Modifiers { shift: false, .. }) if cmd => Some(ShortcutAction::PrevTab),
    (Key::PageDown, Modifiers { shift: false, .. }) if cmd => Some(ShortcutAction::NextTab),

    // Navigation.
    // On macOS, most browsers use Cmd+[ / Cmd+] for back/forward.
    (Key::OpenBracket, Modifiers { shift: false, .. })
      if cmd && matches!(platform, Platform::Mac) =>
    {
      Some(ShortcutAction::Back)
    }
    (Key::CloseBracket, Modifiers { shift: false, .. })
      if cmd && matches!(platform, Platform::Mac) =>
    {
      Some(ShortcutAction::Forward)
    }
    // Home page.
    (
      Key::Home,
      Modifiers {
        ctrl: false,
        shift: false,
        alt: true,
        meta: false,
      },
    ) if matches!(platform, Platform::Other) => Some(ShortcutAction::GoHome),
    (Key::H, Modifiers { shift: true, .. }) if cmd && matches!(platform, Platform::Mac) => {
      Some(ShortcutAction::GoHome)
    }
    (
      Key::Left,
      Modifiers {
        alt: true,
        ctrl: false,
        meta: false,
        ..
      },
    ) if matches!(platform, Platform::Other) => Some(ShortcutAction::Back),
    (
      Key::Right,
      Modifiers {
        alt: true,
        ctrl: false,
        meta: false,
        ..
      },
    ) if matches!(platform, Platform::Other) => Some(ShortcutAction::Forward),
    (Key::R, _) if cmd => Some(ShortcutAction::Reload),
    // F5 should reload even without modifiers. Ignore Ctrl/Cmd+F5 / Alt+F5 for now.
    (
      Key::F5,
      Modifiers {
        alt: false,
        ctrl: false,
        meta: false,
        ..
      },
    ) => Some(ShortcutAction::Reload),

    // Bookmarks / history UI.
    (Key::D, Modifiers { shift: false, .. }) if primary_cmd => Some(ShortcutAction::ToggleBookmark),
    (Key::H, Modifiers { shift: false, .. })
      if primary_cmd && matches!(platform, Platform::Other) =>
    {
      Some(ShortcutAction::ShowHistory)
    }
    // Chrome macOS shortcut for showing History.
    (Key::Y, Modifiers { shift: false, .. })
      if primary_cmd && matches!(platform, Platform::Mac) =>
    {
      Some(ShortcutAction::ShowHistory)
    }
    // Firefox macOS shortcut for showing History (keep both).
    (Key::H, Modifiers { shift: true, .. }) if primary_cmd && matches!(platform, Platform::Mac) => {
      Some(ShortcutAction::ShowHistory)
    }
    (Key::O, Modifiers { shift: true, .. }) if primary_cmd => {
      Some(ShortcutAction::ShowBookmarksManager)
    }

    // UI.
    (Key::B, Modifiers { shift: true, .. }) if cmd => Some(ShortcutAction::ToggleBookmarksBar),
    // Downloads.
    (Key::J, Modifiers { shift: false, .. }) if cmd && matches!(platform, Platform::Other) => {
      Some(ShortcutAction::ToggleDownloadsPanel)
    }
    (Key::J, Modifiers { shift: true, .. }) if primary_cmd && matches!(platform, Platform::Mac) => {
      Some(ShortcutAction::ToggleDownloadsPanel)
    }

    // History / data.
    (Key::Delete, Modifiers { shift: true, .. }) if cmd => {
      Some(ShortcutAction::OpenClearBrowsingDataDialog)
    }

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

    // Save / Print.
    (Key::S, Modifiers { shift: false, .. }) if cmd => Some(ShortcutAction::SavePage),
    (Key::P, Modifiers { shift: false, .. }) if cmd => Some(ShortcutAction::PrintPage),

    // Clipboard.
    // Windows/Linux also support the "IBM Common User Access" variants:
    // - Ctrl+Insert = Copy
    // - Shift+Delete = Cut
    // - Shift+Insert = Paste
    // Keep these separate from the `cmd` mapping so they do not fire on unrelated modifier combos.
    (
      Key::Insert,
      Modifiers {
        ctrl: true,
        shift: false,
        alt: false,
        meta: false,
      },
    ) => Some(ShortcutAction::Copy),
    (
      Key::Delete,
      Modifiers {
        ctrl: false,
        shift: true,
        alt: false,
        meta: false,
      },
    ) => Some(ShortcutAction::Cut),
    (
      Key::Insert,
      Modifiers {
        ctrl: false,
        shift: true,
        alt: false,
        meta: false,
      },
    ) => Some(ShortcutAction::Paste),
    (Key::C, Modifiers { shift: false, .. }) if cmd => Some(ShortcutAction::Copy),
    (Key::X, Modifiers { shift: false, .. }) if cmd => Some(ShortcutAction::Cut),
    (Key::V, Modifiers { shift: false, .. }) if cmd => Some(ShortcutAction::Paste),
    (Key::A, Modifiers { shift: false, .. }) if cmd => Some(ShortcutAction::SelectAll),

    // Bookmarks.
    (Key::O, Modifiers { shift: true, .. }) if cmd => Some(ShortcutAction::ToggleBookmarksManager),

    // Scrolling / page keys.
    (
      Key::PageUp,
      Modifiers {
        ctrl: false,
        shift: false,
        alt: false,
        meta: false,
      },
    ) => Some(ShortcutAction::PageUp),
    (
      Key::PageDown,
      Modifiers {
        ctrl: false,
        shift: false,
        alt: false,
        meta: false,
      },
    ) => Some(ShortcutAction::PageDown),
    // Match `Space` regardless of `shift` so the caller can implement `Shift+Space` scrolling up
    // (common browser behaviour).
    (
      Key::Space,
      Modifiers {
        ctrl: false,
        alt: false,
        meta: false,
        ..
      },
    ) => Some(ShortcutAction::Space),
    (
      Key::Home,
      Modifiers {
        ctrl: false,
        shift: false,
        alt: false,
        meta: false,
      },
    ) => Some(ShortcutAction::Home),
    (
      Key::End,
      Modifiers {
        ctrl: false,
        shift: false,
        alt: false,
        meta: false,
      },
    ) => Some(ShortcutAction::End),

    _ => None,
  }
}

/// Returns true when handling the shortcut should immediately remove page focus.
///
/// Some shortcuts open a chrome-controlled UI surface (address bar, find bar, tab search, downloads
/// panel, etc). The `browser` binary uses this to prevent `WindowEvent::ReceivedCharacter` events
/// (which can arrive before the next egui frame) from being forwarded into the page.
pub fn shortcut_preempts_page_focus(action: ShortcutAction) -> bool {
  matches!(
    action,
    ShortcutAction::FocusAddressBar
      | ShortcutAction::FindInPage
      | ShortcutAction::NewTab
      | ShortcutAction::OpenTabSearch
      | ShortcutAction::ToggleBookmarksManager
      | ShortcutAction::ToggleDownloadsPanel
      | ShortcutAction::ShowBookmarksManager
      | ShortcutAction::ShowHistory
      | ShortcutAction::OpenClearBrowsingDataDialog
  )
}

/// Returns true when the shortcut is reserved for browser chrome and should never be forwarded to
/// page input handling.
///
/// This is used by the windowed browser UI to keep common browser shortcuts (tab management,
/// navigation, zoom, save/print) from leaking into the rendered page.
pub fn shortcut_is_chrome_reserved(action: ShortcutAction) -> bool {
  matches!(
    action,
    ShortcutAction::FocusAddressBar
      | ShortcutAction::FindInPage
      | ShortcutAction::ToggleBookmarksManager
      | ShortcutAction::ToggleDownloadsPanel
      | ShortcutAction::NewWindow
      | ShortcutAction::OpenTabSearch
      | ShortcutAction::NewTab
      | ShortcutAction::CloseTab
      | ShortcutAction::ReopenClosedTab
      | ShortcutAction::NextTab
      | ShortcutAction::PrevTab
      | ShortcutAction::Back
      | ShortcutAction::Forward
      | ShortcutAction::Reload
      | ShortcutAction::GoHome
      | ShortcutAction::ToggleBookmark
      | ShortcutAction::ShowHistory
      | ShortcutAction::ShowBookmarksManager
      | ShortcutAction::ToggleBookmarksBar
      | ShortcutAction::OpenClearBrowsingDataDialog
      | ShortcutAction::ActivateTabNumber(_)
      | ShortcutAction::ZoomIn
      | ShortcutAction::ZoomOut
      | ShortcutAction::ZoomReset
      | ShortcutAction::ToggleFullScreen
      | ShortcutAction::SavePage
      | ShortcutAction::PrintPage
  )
}

#[cfg(test)]
mod tests {
  use super::{
    map_shortcut_with_platform, shortcut_is_chrome_reserved, shortcut_preempts_page_focus, Key,
    KeyEvent, Modifiers, Platform, ShortcutAction,
  };

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
  fn ctrl_f_finds_in_page() {
    assert_eq!(
      map_shortcut_with_platform(
        KeyEvent::new(Key::F, Modifiers::new(true, false, false, false)),
        Platform::Other
      ),
      Some(ShortcutAction::FindInPage)
    );
  }

  #[test]
  fn ctrl_j_toggles_downloads_panel() {
    assert_eq!(
      map_shortcut_with_platform(
        KeyEvent::new(Key::J, Modifiers::new(true, false, false, false)),
        Platform::Other
      ),
      Some(ShortcutAction::ToggleDownloadsPanel)
    );
  }

  #[test]
  fn mac_cmd_shift_j_toggles_downloads_panel() {
    assert_eq!(
      map_shortcut_with_platform(
        KeyEvent::new(Key::J, Modifiers::new(false, true, false, true)),
        Platform::Mac
      ),
      Some(ShortcutAction::ToggleDownloadsPanel)
    );
  }

  #[test]
  fn mac_cmd_f_finds_in_page() {
    assert_eq!(
      map_shortcut_with_platform(
        KeyEvent::new(Key::F, Modifiers::new(false, false, false, true)),
        Platform::Mac
      ),
      Some(ShortcutAction::FindInPage)
    );
  }

  #[test]
  fn f11_toggles_fullscreen_on_other_platforms() {
    assert_eq!(
      map_shortcut_with_platform(
        KeyEvent::new(Key::F11, Modifiers::default()),
        Platform::Other
      ),
      Some(ShortcutAction::ToggleFullScreen)
    );
    // macOS uses Ctrl+Cmd+F for fullscreen; leave F11 unmapped.
    assert_eq!(
      map_shortcut_with_platform(KeyEvent::new(Key::F11, Modifiers::default()), Platform::Mac),
      None
    );
  }

  #[test]
  fn mac_ctrl_cmd_f_toggles_fullscreen() {
    assert_eq!(
      map_shortcut_with_platform(
        KeyEvent::new(Key::F, Modifiers::new(true, false, false, true)),
        Platform::Mac
      ),
      Some(ShortcutAction::ToggleFullScreen)
    );
  }

  #[test]
  fn ctrl_alt_f_does_not_trigger_find_in_page() {
    assert_eq!(
      map_shortcut_with_platform(
        KeyEvent::new(Key::F, Modifiers::new(true, false, true, false)),
        Platform::Other
      ),
      None
    );
  }

  #[test]
  fn alt_d_focuses_address_bar_on_other_platforms() {
    assert_eq!(
      map_shortcut_with_platform(
        KeyEvent::new(Key::D, Modifiers::new(false, false, true, false)),
        Platform::Other
      ),
      Some(ShortcutAction::FocusAddressBar)
    );
    assert_eq!(
      map_shortcut_with_platform(
        KeyEvent::new(Key::D, Modifiers::new(false, false, true, false)),
        Platform::Mac
      ),
      None
    );
  }

  #[test]
  fn f6_focuses_address_bar() {
    assert_eq!(
      map_shortcut_with_platform(
        KeyEvent::new(Key::F6, Modifiers::default()),
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
  fn ctrl_1_through_9_select_tabs() {
    for (n, key) in [
      (1u8, Key::Num1),
      (2, Key::Num2),
      (3, Key::Num3),
      (4, Key::Num4),
      (5, Key::Num5),
      (6, Key::Num6),
      (7, Key::Num7),
      (8, Key::Num8),
      (9, Key::Num9),
    ] {
      assert_eq!(
        map_shortcut_with_platform(
          KeyEvent::new(key, Modifiers::new(true, false, false, false)),
          Platform::Other
        ),
        Some(ShortcutAction::ActivateTabNumber(n)),
        "expected Ctrl+{n} to map to ActivateTabNumber({n})"
      );
    }
  }

  #[test]
  fn shift_insert_pastes() {
    assert_eq!(
      map_shortcut_with_platform(
        KeyEvent::new(Key::Insert, Modifiers::new(false, true, false, false)),
        Platform::Other
      ),
      Some(ShortcutAction::Paste)
    );
  }

  #[test]
  fn ctrl_insert_copies() {
    assert_eq!(
      map_shortcut_with_platform(
        KeyEvent::new(Key::Insert, Modifiers::new(true, false, false, false)),
        Platform::Other
      ),
      Some(ShortcutAction::Copy)
    );
  }

  #[test]
  fn shift_delete_cuts() {
    assert_eq!(
      map_shortcut_with_platform(
        KeyEvent::new(Key::Delete, Modifiers::new(false, true, false, false)),
        Platform::Other
      ),
      Some(ShortcutAction::Cut)
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
  fn ctrl_n_new_window() {
    assert_eq!(
      map_shortcut_with_platform(
        KeyEvent::new(Key::N, Modifiers::new(true, false, false, false)),
        Platform::Other
      ),
      Some(ShortcutAction::NewWindow)
    );
  }

  #[test]
  fn mac_cmd_n_new_window() {
    assert_eq!(
      map_shortcut_with_platform(
        KeyEvent::new(Key::N, Modifiers::new(false, false, false, true)),
        Platform::Mac
      ),
      Some(ShortcutAction::NewWindow)
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
  fn ctrl_f4_close_tab() {
    assert_eq!(
      map_shortcut_with_platform(
        KeyEvent::new(Key::F4, Modifiers::new(true, false, false, false)),
        Platform::Other
      ),
      Some(ShortcutAction::CloseTab)
    );
    // Alt+F4 is typically reserved for window management; it should not be treated as a browser
    // shortcut.
    assert_eq!(
      map_shortcut_with_platform(
        KeyEvent::new(Key::F4, Modifiers::new(false, false, true, false)),
        Platform::Other
      ),
      None
    );
    // On macOS, treat Cmd+F4 the same way as Cmd+W.
    assert_eq!(
      map_shortcut_with_platform(
        KeyEvent::new(Key::F4, Modifiers::new(false, false, false, true)),
        Platform::Mac
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
  fn ctrl_pageup_pagedown_cycle_tabs() {
    assert_eq!(
      map_shortcut_with_platform(
        KeyEvent::new(Key::PageDown, Modifiers::new(true, false, false, false)),
        Platform::Other
      ),
      Some(ShortcutAction::NextTab)
    );
    assert_eq!(
      map_shortcut_with_platform(
        KeyEvent::new(Key::PageUp, Modifiers::new(true, false, false, false)),
        Platform::Other
      ),
      Some(ShortcutAction::PrevTab)
    );
    // Shift should suppress this mapping so Shift+PageDown can remain a selection/navigation key.
    assert_eq!(
      map_shortcut_with_platform(
        KeyEvent::new(Key::PageDown, Modifiers::new(true, true, false, false)),
        Platform::Other
      ),
      None
    );
  }

  #[test]
  fn mac_cmd_pageup_pagedown_cycle_tabs() {
    assert_eq!(
      map_shortcut_with_platform(
        KeyEvent::new(Key::PageDown, Modifiers::new(false, false, false, true)),
        Platform::Mac
      ),
      Some(ShortcutAction::NextTab)
    );
    assert_eq!(
      map_shortcut_with_platform(
        KeyEvent::new(Key::PageUp, Modifiers::new(false, false, false, true)),
        Platform::Mac
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
    // On macOS, avoid using Alt+Left/Right for history navigation (Option+Arrow is commonly used for
    // word-wise movement in text controls).
    assert_eq!(
      map_shortcut_with_platform(
        KeyEvent::new(Key::Left, Modifiers::new(false, false, true, false)),
        Platform::Mac
      ),
      None
    );
    assert_eq!(
      map_shortcut_with_platform(
        KeyEvent::new(Key::Right, Modifiers::new(false, false, true, false)),
        Platform::Mac
      ),
      None
    );
  }

  #[test]
  fn mac_cmd_brackets_navigate_history() {
    assert_eq!(
      map_shortcut_with_platform(
        KeyEvent::new(Key::OpenBracket, Modifiers::new(false, false, false, true)),
        Platform::Mac
      ),
      Some(ShortcutAction::Back)
    );
    assert_eq!(
      map_shortcut_with_platform(
        KeyEvent::new(Key::CloseBracket, Modifiers::new(false, false, false, true)),
        Platform::Mac
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
    // Ensure we don't treat AltGr as chrome-level Ctrl shortcuts.
    for key in [
      Key::B,
      Key::F,
      Key::L,
      Key::N,
      Key::Tab,
      Key::Num1,
      Key::Equals,
      Key::Minus,
      Key::R,
      Key::S,
      Key::P,
      Key::Delete,
      Key::F11,
    ] {
      assert_eq!(
        map_shortcut_with_platform(
          KeyEvent::new(key, Modifiers::new(true, false, true, false)),
          Platform::Other
        ),
        None,
        "expected Ctrl+Alt+{key:?} to not map to a browser shortcut"
      );
    }
    // Ensure AltGr doesn't trigger Alt+Left/Right history navigation.
    assert_eq!(
      map_shortcut_with_platform(
        KeyEvent::new(Key::Left, Modifiers::new(true, false, true, false)),
        Platform::Other
      ),
      None
    );
  }

  #[test]
  fn alt_home_goes_to_browser_home_on_other_platforms() {
    assert_eq!(
      map_shortcut_with_platform(
        KeyEvent::new(Key::Home, Modifiers::new(false, false, true, false)),
        Platform::Other
      ),
      Some(ShortcutAction::GoHome)
    );
    // AltGr (often Ctrl+Alt) should not trigger this shortcut.
    assert_eq!(
      map_shortcut_with_platform(
        KeyEvent::new(Key::Home, Modifiers::new(true, false, true, false)),
        Platform::Other
      ),
      None
    );
  }

  #[test]
  fn ctrl_shift_a_opens_tab_search() {
    assert_eq!(
      map_shortcut_with_platform(
        KeyEvent::new(Key::A, Modifiers::new(true, true, false, false)),
        Platform::Other
      ),
      Some(ShortcutAction::OpenTabSearch)
    );
  }

  #[test]
  fn mac_cmd_shift_h_goes_to_browser_home() {
    assert_eq!(
      map_shortcut_with_platform(
        KeyEvent::new(Key::H, Modifiers::new(false, true, false, true)),
        Platform::Mac
      ),
      Some(ShortcutAction::GoHome)
    );
  }

  #[test]
  fn mac_cmd_shift_a_opens_tab_search() {
    assert_eq!(
      map_shortcut_with_platform(
        KeyEvent::new(Key::A, Modifiers::new(false, true, false, true)),
        Platform::Mac
      ),
      Some(ShortcutAction::OpenTabSearch)
    );
  }

  #[test]
  fn ctrl_shift_a_is_not_select_all() {
    assert_ne!(
      map_shortcut_with_platform(
        KeyEvent::new(Key::A, Modifiers::new(true, true, false, false)),
        Platform::Other
      ),
      Some(ShortcutAction::SelectAll)
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

  #[test]
  fn ctrl_d_toggles_bookmark() {
    assert_eq!(
      map_shortcut_with_platform(
        KeyEvent::new(Key::D, Modifiers::new(true, false, false, false)),
        Platform::Other
      ),
      Some(ShortcutAction::ToggleBookmark)
    );
  }

  #[test]
  fn ctrl_shift_b_toggles_bookmarks_bar() {
    assert_eq!(
      map_shortcut_with_platform(
        KeyEvent::new(Key::B, Modifiers::new(true, true, false, false)),
        Platform::Other
      ),
      Some(ShortcutAction::ToggleBookmarksBar)
    );
  }

  #[test]
  fn ctrl_h_shows_history_on_other_platforms() {
    assert_eq!(
      map_shortcut_with_platform(
        KeyEvent::new(Key::H, Modifiers::new(true, false, false, false)),
        Platform::Other
      ),
      Some(ShortcutAction::ShowHistory)
    );
  }

  #[test]
  fn ctrl_shift_o_shows_bookmarks_manager() {
    assert_eq!(
      map_shortcut_with_platform(
        KeyEvent::new(Key::O, Modifiers::new(true, true, false, false)),
        Platform::Other
      ),
      Some(ShortcutAction::ShowBookmarksManager)
    );
  }

  #[test]
  fn mac_cmd_or_ctrl_shift_b_toggles_bookmarks_bar() {
    assert_eq!(
      map_shortcut_with_platform(
        KeyEvent::new(Key::B, Modifiers::new(false, true, false, true)),
        Platform::Mac
      ),
      Some(ShortcutAction::ToggleBookmarksBar)
    );
    // Allow Ctrl as a secondary shortcut modifier on macOS.
    assert_eq!(
      map_shortcut_with_platform(
        KeyEvent::new(Key::B, Modifiers::new(true, true, false, false)),
        Platform::Mac
      ),
      Some(ShortcutAction::ToggleBookmarksBar)
    );
  }

  #[test]
  fn ctrl_shift_delete_opens_clear_browsing_data_dialog() {
    assert_eq!(
      map_shortcut_with_platform(
        KeyEvent::new(Key::Delete, Modifiers::new(true, true, false, false)),
        Platform::Other
      ),
      Some(ShortcutAction::OpenClearBrowsingDataDialog)
    );
    // Ensure we still treat Shift+Delete without Ctrl/Cmd as Cut (IBM CUA shortcut).
    assert_eq!(
      map_shortcut_with_platform(
        KeyEvent::new(Key::Delete, Modifiers::new(false, true, false, false)),
        Platform::Other
      ),
      Some(ShortcutAction::Cut)
    );
  }

  #[test]
  fn mac_cmd_d_toggles_bookmark_and_ctrl_d_is_ignored() {
    assert_eq!(
      map_shortcut_with_platform(
        KeyEvent::new(Key::D, Modifiers::new(false, false, false, true)),
        Platform::Mac
      ),
      Some(ShortcutAction::ToggleBookmark)
    );
    // Ctrl+D is commonly "forward delete" in macOS text controls; do not treat it as bookmark.
    assert_eq!(
      map_shortcut_with_platform(
        KeyEvent::new(Key::D, Modifiers::new(true, false, false, false)),
        Platform::Mac
      ),
      None
    );
  }

  #[test]
  fn mac_cmd_y_shows_history_and_ctrl_h_is_ignored() {
    assert_eq!(
      map_shortcut_with_platform(
        KeyEvent::new(Key::Y, Modifiers::new(false, false, false, true)),
        Platform::Mac
      ),
      Some(ShortcutAction::ShowHistory)
    );
    // Ctrl+H is a common macOS editing keybinding; do not treat it as History.
    assert_eq!(
      map_shortcut_with_platform(
        KeyEvent::new(Key::H, Modifiers::new(true, false, false, false)),
        Platform::Mac
      ),
      None
    );
  }

  #[test]
  fn mac_cmd_or_ctrl_shift_delete_opens_clear_browsing_data_dialog() {
    assert_eq!(
      map_shortcut_with_platform(
        KeyEvent::new(Key::Delete, Modifiers::new(false, true, false, true)),
        Platform::Mac
      ),
      Some(ShortcutAction::OpenClearBrowsingDataDialog)
    );
    // Allow Ctrl as a secondary shortcut modifier on macOS.
    assert_eq!(
      map_shortcut_with_platform(
        KeyEvent::new(Key::Delete, Modifiers::new(true, true, false, false)),
        Platform::Mac
      ),
      Some(ShortcutAction::OpenClearBrowsingDataDialog)
    );
  }

  #[test]
  fn shortcuts_that_preempt_page_focus() {
    for action in [
      ShortcutAction::FocusAddressBar,
      ShortcutAction::FindInPage,
      ShortcutAction::NewTab,
      ShortcutAction::OpenTabSearch,
      ShortcutAction::ToggleBookmarksManager,
      ShortcutAction::ToggleDownloadsPanel,
      ShortcutAction::ShowBookmarksManager,
      ShortcutAction::ShowHistory,
      ShortcutAction::OpenClearBrowsingDataDialog,
    ] {
      assert!(
        shortcut_preempts_page_focus(action),
        "{action:?} should preempt page focus"
      );
    }
  }

  #[test]
  fn other_shortcuts_do_not_preempt_page_focus() {
    for action in [
      ShortcutAction::Back,
      ShortcutAction::Forward,
      ShortcutAction::Reload,
      ShortcutAction::GoHome,
      ShortcutAction::NextTab,
      ShortcutAction::PrevTab,
      ShortcutAction::ActivateTabNumber(1),
      ShortcutAction::ZoomIn,
      ShortcutAction::ZoomOut,
      ShortcutAction::ZoomReset,
      ShortcutAction::ToggleFullScreen,
      ShortcutAction::SavePage,
      ShortcutAction::PrintPage,
      ShortcutAction::Copy,
      ShortcutAction::Cut,
      ShortcutAction::Paste,
      ShortcutAction::SelectAll,
      ShortcutAction::PageUp,
      ShortcutAction::PageDown,
      ShortcutAction::Space,
      ShortcutAction::Home,
      ShortcutAction::End,
    ] {
      assert!(
        !shortcut_preempts_page_focus(action),
        "{action:?} should not preempt page focus"
      );
    }
  }

  #[test]
  fn ctrl_s_saves_page() {
    assert_eq!(
      map_shortcut_with_platform(
        KeyEvent::new(Key::S, Modifiers::new(true, false, false, false)),
        Platform::Other
      ),
      Some(ShortcutAction::SavePage)
    );
  }

  #[test]
  fn ctrl_p_prints_page() {
    assert_eq!(
      map_shortcut_with_platform(
        KeyEvent::new(Key::P, Modifiers::new(true, false, false, false)),
        Platform::Other
      ),
      Some(ShortcutAction::PrintPage)
    );
  }

  #[test]
  fn mac_cmd_s_saves_page() {
    assert_eq!(
      map_shortcut_with_platform(
        KeyEvent::new(Key::S, Modifiers::new(false, false, false, true)),
        Platform::Mac
      ),
      Some(ShortcutAction::SavePage)
    );
  }

  #[test]
  fn mac_cmd_p_prints_page() {
    assert_eq!(
      map_shortcut_with_platform(
        KeyEvent::new(Key::P, Modifiers::new(false, false, false, true)),
        Platform::Mac
      ),
      Some(ShortcutAction::PrintPage)
    );
  }

  #[test]
  fn save_and_print_shortcuts_are_chrome_reserved() {
    for action in [ShortcutAction::SavePage, ShortcutAction::PrintPage] {
      assert!(
        shortcut_is_chrome_reserved(action),
        "{action:?} should be reserved by browser chrome"
      );
    }
  }

  #[test]
  fn downloads_panel_shortcut_is_chrome_reserved() {
    assert!(
      shortcut_is_chrome_reserved(ShortcutAction::ToggleDownloadsPanel),
      "ToggleDownloadsPanel should be reserved by browser chrome"
    );
  }
}
