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

/// Accessible label for the rendered page viewport widget (the egui widget that hosts the page).
pub const PAGE_VIEWPORT_LABEL: &str = "Web page";

fn normalize_page_title(title: &str) -> String {
  title
    .trim()
    .split_whitespace()
    .collect::<Vec<_>>()
    .join(" ")
}

/// Return the accessible name for the browser's page viewport widget.
///
/// When a document title is available, it is included so screen readers can quickly identify which
/// tab/document is currently focused.
pub fn page_viewport_accessible_name(document_title: Option<&str>) -> String {
  let title = document_title
    .map(normalize_page_title)
    .filter(|t| !t.is_empty());
  match title {
    Some(title) => format!("{PAGE_VIEWPORT_LABEL}: {title}"),
    None => PAGE_VIEWPORT_LABEL.to_string(),
  }
}

/// Configure accessibility metadata for the egui widget that hosts the rendered page.
///
/// This ensures that:
/// - the host node uses a container role (`WebView`) so injected document subtree nodes can be
///   traversed by assistive technology, and
/// - the node has a stable accessible name.
pub fn configure_page_viewport_accessibility(
  response: &egui::Response,
  document_title: Option<&str>,
) {
  let label = page_viewport_accessible_name(document_title);

  response.widget_info({
    let label = label.clone();
    move || egui::WidgetInfo::labeled(egui::WidgetType::Label, label.clone())
  });

  let _ = response
    .ctx
    .accesskit_node_builder(response.id, move |builder| {
      builder.set_role(accesskit::Role::WebView);
      // Some assistive tech expects `Focus` to be explicitly exposed for focusable container nodes.
      builder.add_action(accesskit::Action::Focus);
    });
}

#[cfg(test)]
mod tests {
  use crate::ui::a11y_test_util;

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

  #[test]
  fn page_viewport_widget_is_exposed_as_web_view_in_accesskit() {
    let ctx = egui::Context::default();
    // AccessKit output is typically enabled by the platform adapter (egui-winit). In headless unit
    // tests we force it on to ensure egui emits an update.
    ctx.enable_accesskit();

    let mut raw = egui::RawInput::default();
    raw.focused = true;
    raw.time = Some(0.0);
    raw.pixels_per_point = Some(1.0);
    raw.screen_rect = Some(egui::Rect::from_min_size(
      egui::pos2(0.0, 0.0),
      egui::vec2(800.0, 600.0),
    ));
    ctx.begin_frame(raw);

    let expected_name = super::page_viewport_accessible_name(Some("Example document"));

    egui::CentralPanel::default().show(&ctx, |ui| {
      let response = ui.add(
        egui::Image::new((egui::TextureId::Managed(0), egui::vec2(100.0, 100.0)))
          .sense(egui::Sense::click()),
      );
      super::configure_page_viewport_accessibility(&response, Some("Example document"));
      response.request_focus();
    });

    let output = ctx.end_frame();
    let update = output
      .platform_output
      .accesskit_update
      .as_ref()
      .expect("expected egui to emit an AccessKit update");

    let node = update
      .nodes
      .iter()
      .find_map(|(_id, node)| {
        let name = node.name().unwrap_or("").trim();
        (name == expected_name).then_some(node)
      })
      .unwrap_or_else(|| {
        let snapshot = a11y_test_util::accesskit_pretty_json_from_full_output(&output);
        panic!(
          "expected to find AccessKit node with name {expected_name:?}.\n\nsnapshot:\n{snapshot}"
        );
      });

    assert_eq!(
      node.role(),
      accesskit::Role::WebView,
      "expected page viewport node to use a container role"
    );
    assert!(
      node.supports_action(accesskit::Action::Focus),
      "expected page viewport node to remain focusable"
    );
  }
}
