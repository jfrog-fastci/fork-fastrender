#![cfg(feature = "browser_ui")]

/// Accessibility helpers for the egui-based browser chrome.
///
/// The windowed browser UI (`src/bin/browser.rs`) uses `egui-winit` with AccessKit enabled. This
/// allows egui to expose its widget tree to screen readers (VoiceOver/Narrator/Orca).
///
/// For a detailed architecture + debugging guide (coordinate conventions, NodeId stability rules,
/// action routing, and `dump_accesskit`), see `docs/chrome_accessibility.md`.
///
/// Most icon-only chrome controls should use `crate::ui::BrowserIcon` + `crate::ui::icon_button`
/// so they automatically get stable hover text + AccessKit labels via `BrowserIcon::a11y_label`.
///
/// This module contains shared labels for non-icon widgets.

pub const ADDRESS_BAR_LABEL: &str = "Address bar";
pub const TAB_SEARCH_LABEL: &str = "Search tabs";
pub const FIND_IN_PAGE_LABEL: &str = "Find in page";
pub const HISTORY_PANEL_SEARCH_LABEL: &str = "Search history";
pub const BOOKMARKS_MANAGER_SEARCH_LABEL: &str = "Search bookmarks";

#[cfg(test)]
mod tests {
  #[test]
  fn a11y_accesskit_crates_linked_when_browser_ui_enabled() {
    // This test is intentionally "dumb": it just references a couple of AccessKit symbols so the
    // linker fails loudly if the browser UI stack drops accessibility support.
    let _role = accesskit::Role::Button;
    let _ = std::any::TypeId::of::<accesskit_winit::Adapter>();
  }

  #[test]
  fn a11y_shared_labels_are_non_empty() {
    assert!(!super::ADDRESS_BAR_LABEL.trim().is_empty());
    assert!(!super::TAB_SEARCH_LABEL.trim().is_empty());
    assert!(!super::FIND_IN_PAGE_LABEL.trim().is_empty());
    assert!(!super::HISTORY_PANEL_SEARCH_LABEL.trim().is_empty());
    assert!(!super::BOOKMARKS_MANAGER_SEARCH_LABEL.trim().is_empty());
  }
}
