//! AccessKit integration for page (rendered document) nodes.
//!
//! The windowed browser UI renders chrome widgets with egui and page content via the render worker.
//! Assistive technology uses AccessKit `ActionRequest`s to drive interactions. egui handles action
//! requests targeting chrome widgets, while page-targeted requests need to be forwarded to the
//! render worker.
//!
//! This module provides:
//! - Helpers to split page action requests out of egui `RawInput`.
//! - Mapping from AccessKit `ActionRequest`s to `UiToWorker` messages.

#![cfg(feature = "browser_ui")]

use crate::ui::{decode_page_node_id, page_accesskit_ids, TabId, UiToWorker};

fn decode_page_target_node_id(target: accesskit::NodeId) -> Option<(TabId, usize)> {
  // Prefer the legacy tag-bit encoding first so we don't accidentally treat it as the canonical
  // `(tab_id, generation, dom_node_id)` encoding (which would yield a bogus tab id).
  if page_accesskit_ids::is_page_node_id(target) {
    return page_accesskit_ids::decode_page_node_id(target);
  }

  decode_page_node_id(target).map(|(tab_id, _generation, dom_node_id)| (tab_id, dom_node_id))
}

/// Drain AccessKit action requests targeting *page* nodes out of egui `RawInput`.
///
/// The returned requests should be handled by the browser UI and forwarded to the render worker.
/// All non-page requests remain in `raw_input` so egui can continue handling chrome accessibility
/// actions (e.g. expand/collapse toggles).
pub fn drain_page_accesskit_action_requests(
  raw_input: &mut egui::RawInput,
) -> Vec<accesskit::ActionRequest> {
  let mut page_reqs = Vec::new();
  let mut retained_events = Vec::with_capacity(raw_input.events.len());

  for event in std::mem::take(&mut raw_input.events) {
    match event {
      egui::Event::AccessKitActionRequest(req) => {
        if decode_page_target_node_id(req.target).is_some() {
          page_reqs.push(req);
        } else {
          retained_events.push(egui::Event::AccessKitActionRequest(req));
        }
      }
      other => retained_events.push(other),
    }
  }

  raw_input.events = retained_events;
  page_reqs
}

/// Map a single AccessKit `ActionRequest` into a UI→worker message.
///
/// This returns `None` when:
/// - the target node id is not a page node id (egui chrome ids),
/// - the request is malformed (missing required `data` payload),
/// - the action is unsupported by the browser UI.
pub fn action_request_to_ui_message(req: &accesskit::ActionRequest) -> Option<UiToWorker> {
  let (tab_id, node_id) = decode_page_target_node_id(req.target)?;

  match req.action {
    accesskit::Action::Focus => Some(UiToWorker::A11ySetFocus { tab_id, node_id }),
    // "Default" is AccessKit's generic "activate" action (click/press). Some platforms use
    // `Click` instead.
    accesskit::Action::Click | accesskit::Action::Default => {
      Some(UiToWorker::A11yActivate { tab_id, node_id })
    }
    accesskit::Action::ScrollIntoView => Some(UiToWorker::A11yScrollIntoView { tab_id, node_id }),
    accesskit::Action::ShowContextMenu => Some(UiToWorker::A11yShowContextMenu {
      tab_id,
      node_id: Some(node_id),
    }),
    accesskit::Action::SetValue => {
      let value = match req.data.as_ref()? {
        accesskit::ActionData::Value(value) => value.to_string(),
        _ => return None,
      };
      Some(UiToWorker::A11ySetTextValue {
        tab_id,
        node_id,
        value,
      })
    }
    accesskit::Action::SetTextSelection => {
      let selection = match req.data.as_ref()? {
        accesskit::ActionData::SetTextSelection(selection) => selection,
        _ => return None,
      };

      // Defensive: AccessKit selection ranges should refer to the same text control node. Ignore
      // malformed requests rather than applying an unexpected selection range.
      if selection.anchor.node != req.target || selection.focus.node != req.target {
        return None;
      }

      let anchor = selection.anchor.character_index;
      let focus = selection.focus.character_index;
      Some(UiToWorker::A11ySetTextSelectionRange {
        tab_id,
        node_id,
        anchor,
        focus,
      })
    }
    _ => None,
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn action_request_to_ui_message_focus_maps_to_focus_node() {
    let tab_id = TabId(42);
    let target = crate::ui::encode_page_node_id(tab_id, 1, 7);
    let req = accesskit::ActionRequest {
      action: accesskit::Action::Focus,
      target,
      data: None,
    };
    assert!(matches!(
      action_request_to_ui_message(&req),
      Some(UiToWorker::A11ySetFocus {
        tab_id: got_tab,
        node_id: 7
      }) if got_tab == tab_id
    ));
  }

  #[test]
  fn action_request_to_ui_message_default_maps_to_activate_node() {
    let tab_id = TabId(1);
    let target = crate::ui::encode_page_node_id(tab_id, 1, 99);
    let req = accesskit::ActionRequest {
      action: accesskit::Action::Default,
      target,
      data: None,
    };
    assert!(matches!(
      action_request_to_ui_message(&req),
      Some(UiToWorker::A11yActivate {
        tab_id: got_tab,
        node_id: 99
      }) if got_tab == tab_id
    ));
  }

  #[test]
  fn action_request_to_ui_message_scroll_into_view_maps_to_scroll_into_view() {
    let tab_id = TabId(3);
    let target = crate::ui::encode_page_node_id(tab_id, 1, 123);
    let req = accesskit::ActionRequest {
      action: accesskit::Action::ScrollIntoView,
      target,
      data: None,
    };
    assert!(matches!(
      action_request_to_ui_message(&req),
      Some(UiToWorker::A11yScrollIntoView {
        tab_id: got_tab,
        node_id: 123
      }) if got_tab == tab_id
    ));
  }

  #[test]
  fn action_request_to_ui_message_set_value_maps_to_set_value() {
    let tab_id = TabId(9);
    let target = crate::ui::encode_page_node_id(tab_id, 1, 5);
    let req = accesskit::ActionRequest {
      action: accesskit::Action::SetValue,
      target,
      data: Some(accesskit::ActionData::Value("hello".into())),
    };
    assert!(matches!(
      action_request_to_ui_message(&req),
      Some(UiToWorker::A11ySetTextValue { tab_id: got_tab, node_id: 5, value })
        if got_tab == tab_id && value == "hello"
    ));
  }

  #[test]
  fn action_request_to_ui_message_set_selection_maps_to_set_selection() {
    let tab_id = TabId(2);
    let target = crate::ui::encode_page_node_id(tab_id, 1, 11);
    let req = accesskit::ActionRequest {
      action: accesskit::Action::SetTextSelection,
      target,
      data: Some(accesskit::ActionData::SetTextSelection(accesskit::TextSelection {
        anchor: accesskit::TextPosition {
          node: target,
          character_index: 1,
        },
        focus: accesskit::TextPosition {
          node: target,
          character_index: 4,
        },
      })),
    };
    assert!(matches!(
      action_request_to_ui_message(&req),
      Some(UiToWorker::A11ySetTextSelectionRange {
        tab_id: got_tab,
        node_id: 11,
        anchor: 1,
        focus: 4
      }) if got_tab == tab_id
    ));
  }

  #[test]
  fn action_request_to_ui_message_ignores_non_page_node_ids() {
    let target = accesskit::NodeId(std::num::NonZeroU128::new(123).unwrap());
    let req = accesskit::ActionRequest {
      action: accesskit::Action::Focus,
      target,
      data: None,
    };
    assert!(action_request_to_ui_message(&req).is_none());
  }

  #[test]
  fn drain_page_accesskit_action_requests_preserves_chrome_requests() {
    let tab_id = TabId(5);
    let page_target = crate::ui::encode_page_node_id(tab_id, 1, 7);
    let page_req = accesskit::ActionRequest {
      action: accesskit::Action::Focus,
      target: page_target,
      data: None,
    };

    let chrome_target = accesskit::NodeId(std::num::NonZeroU128::new(999).unwrap());
    let chrome_req = accesskit::ActionRequest {
      action: accesskit::Action::Expand,
      target: chrome_target,
      data: None,
    };

    let mut raw = egui::RawInput::default();
    raw.events = vec![
      egui::Event::AccessKitActionRequest(page_req),
      egui::Event::AccessKitActionRequest(chrome_req),
    ];

    let page = drain_page_accesskit_action_requests(&mut raw);
    assert_eq!(page.len(), 1);
    assert_eq!(raw.events.len(), 1);
    match &raw.events[0] {
      egui::Event::AccessKitActionRequest(req) => {
        assert_eq!(req.action, accesskit::Action::Expand);
      }
      other => panic!("expected AccessKitActionRequest event, got {other:?}"),
    }
  }
}
