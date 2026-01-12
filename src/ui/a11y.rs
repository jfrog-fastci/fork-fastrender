#![cfg(feature = "browser_ui")]

/// Accessibility helpers for the egui-based browser chrome.
///
/// The windowed browser UI (`src/bin/browser.rs`) uses `egui-winit` with the `accesskit` feature
/// enabled. This allows egui to expose its widget tree to screen readers (VoiceOver/Narrator/Orca)
/// via AccessKit.
///
/// The primary UX gap for screen readers in the chrome is icon-only controls: without an explicit
/// accessible name, assistive tech will often announce the raw glyph ("left arrow", "+", "×", ...)
/// instead of a semantic label ("Back", "New tab", "Close tab", ...).
///
/// This module provides a small set of shared labels and helpers to keep those names consistent and
/// testable.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChromeIconButton {
  Back,
  Forward,
  Reload,
  NewTab,
  CloseTab,
  ZoomOut,
  ZoomIn,
}

impl ChromeIconButton {
  pub const fn all() -> &'static [ChromeIconButton] {
    &[
      ChromeIconButton::Back,
      ChromeIconButton::Forward,
      ChromeIconButton::Reload,
      ChromeIconButton::NewTab,
      ChromeIconButton::CloseTab,
      ChromeIconButton::ZoomOut,
      ChromeIconButton::ZoomIn,
    ]
  }

  pub const fn icon(self) -> &'static str {
    match self {
      ChromeIconButton::Back => "←",
      ChromeIconButton::Forward => "→",
      ChromeIconButton::Reload => "⟳",
      ChromeIconButton::NewTab => "+",
      ChromeIconButton::CloseTab => "×",
      ChromeIconButton::ZoomOut => "−",
      ChromeIconButton::ZoomIn => "+",
    }
  }

  pub const fn label(self) -> &'static str {
    match self {
      ChromeIconButton::Back => "Back",
      ChromeIconButton::Forward => "Forward",
      ChromeIconButton::Reload => "Reload",
      ChromeIconButton::NewTab => "New tab",
      ChromeIconButton::CloseTab => "Close tab",
      ChromeIconButton::ZoomOut => "Zoom out",
      ChromeIconButton::ZoomIn => "Zoom in",
    }
  }
}

pub const ADDRESS_BAR_LABEL: &str = "Address bar";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IconButtonSize {
  Normal,
  Small,
}

pub fn chrome_icon_button_widget(
  kind: ChromeIconButton,
  size: IconButtonSize,
) -> egui::Button<'static> {
  let mut button = egui::Button::new(kind.icon());
  if matches!(size, IconButtonSize::Small) {
    button = button.small();
  }
  button
}

pub fn label_icon_button(response: egui::Response, kind: ChromeIconButton) -> egui::Response {
  let label = kind.label();
  let response = response.on_hover_text(label);
  response.widget_info(move || egui::WidgetInfo::labeled(egui::WidgetType::Button, label));
  response
}

pub fn chrome_icon_button(
  ui: &mut egui::Ui,
  kind: ChromeIconButton,
  size: IconButtonSize,
) -> egui::Response {
  let response = ui.add(chrome_icon_button_widget(kind, size));
  label_icon_button(response, kind)
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn a11y_accesskit_crates_linked_when_browser_ui_enabled() {
    // This test is intentionally "dumb": it just references a couple of AccessKit symbols so the
    // linker fails loudly if the browser UI stack drops accessibility support.
    let _role = accesskit::Role::Button;
    let _ = std::any::TypeId::of::<accesskit_winit::Adapter>();
  }

  #[test]
  fn a11y_chrome_icon_buttons_have_accessible_labels() {
    for kind in ChromeIconButton::all() {
      let label = kind.label();
      assert!(
        !label.trim().is_empty(),
        "expected {:?} to have a non-empty label",
        kind
      );
    }
  }
}
