#![cfg(feature = "browser_ui")]

use crate::dom::DomNode;
use crate::interaction::dom_index::DomIndex;
use crate::interaction::{InteractionAction, InteractionEngine, KeyAction};
use crate::scroll::ScrollState;
use crate::tree::box_tree::BoxTree;
use crate::tree::fragment_tree::FragmentTree;
use std::num::NonZeroU128;

/// Shared context for routing AccessKit [`accesskit::ActionRequest`]s into FastRender's interaction
/// engine.
///
/// The windowed browser UI can use this to translate screen reader requests (e.g. "press" on a
/// button) into the same internal operations used by pointer/keyboard input.
pub struct ChromeDocumentContext<'a> {
  pub dom: &'a mut DomNode,
  pub interaction: &'a mut InteractionEngine,

  pub box_tree: Option<&'a BoxTree>,
  pub fragment_tree: Option<&'a FragmentTree>,
  pub scroll_state: Option<&'a ScrollState>,

  pub document_url: &'a str,
  pub base_url: &'a str,

  /// Set to `true` when an action changes interaction/DOM state and the caller should re-render.
  pub needs_redraw: &'a mut bool,

  /// Optional sink for high-level interaction actions (navigation, open dropdown, etc).
  pub emitted_actions: Option<&'a mut Vec<InteractionAction>>,
}

impl ChromeDocumentContext<'_> {
  fn push_action(&mut self, action: InteractionAction) {
    if matches!(action, InteractionAction::None) {
      return;
    }
    if let Some(out) = self.emitted_actions.as_mut() {
      out.push(action);
    }
  }

  fn mark_redraw(&mut self) {
    *self.needs_redraw = true;
  }
}

/// Encode a FastRender DOM pre-order node id into an AccessKit [`accesskit::NodeId`].
///
/// FastRender uses stable 1-indexed pre-order ids (matching `crate::dom::enumerate_dom_ids`).
/// AccessKit requires a non-zero `u128`, so we store the id directly.
pub fn accesskit_node_id_from_fastrender(node_id: usize) -> accesskit::NodeId {
  let raw = NonZeroU128::new(node_id as u128).expect("DOM node ids are 1-indexed");
  accesskit::NodeId(raw)
}

/// Decode an AccessKit [`accesskit::NodeId`] into a FastRender DOM pre-order node id.
pub fn fastrender_node_id_from_accesskit(node_id: accesskit::NodeId) -> Option<usize> {
  let raw = node_id.0.get();
  if raw == 0 {
    return None;
  }
  if raw > usize::MAX as u128 {
    return None;
  }
  Some(raw as usize)
}

/// Route a single AccessKit [`accesskit::ActionRequest`] into FastRender's [`InteractionEngine`].
///
/// Returns `true` when the request was recognized and dispatched.
pub fn handle_accesskit_action_request(
  ctx: &mut ChromeDocumentContext<'_>,
  request: accesskit::ActionRequest,
) -> bool {
  let Some(target_node_id) = fastrender_node_id_from_accesskit(request.target) else {
    return false;
  };

  match request.action {
    accesskit::Action::Focus => {
      let (changed, action) = ctx
        .interaction
        .focus_node_id(ctx.dom, Some(target_node_id), true);
      ctx.push_action(action);
      if changed {
        ctx.mark_redraw();
      }
      true
    }
    // AccessKit 0.11 exposes "press"/"click" semantics via `Action::Default`.
    accesskit::Action::Default => {
      // Ensure the target is focused before activation (matching the pointer/keyboard path, where
      // activation is based on the focused element).
      let (focus_changed, focus_action) = ctx
        .interaction
        .focus_node_id(ctx.dom, Some(target_node_id), true);
      ctx.push_action(focus_action);

      let (changed, action) = ctx.interaction.key_activate(
        ctx.dom,
        KeyAction::Enter,
        ctx.document_url,
        ctx.base_url,
      );
      ctx.push_action(action);

      if focus_changed || changed {
        ctx.mark_redraw();
      }
      true
    }
    accesskit::Action::SetValue => {
      let Some(data) = request.data else {
        return false;
      };
      let value = match data {
        accesskit::ActionData::Value(value) => value,
        _ => return false,
      };

      let (focus_changed, focus_action) = ctx
        .interaction
        .focus_node_id(ctx.dom, Some(target_node_id), true);
      ctx.push_action(focus_action);

      let changed = ctx
        .interaction
        .set_text_control_value(ctx.dom, target_node_id, &value);

      if focus_changed || changed {
        ctx.mark_redraw();
      }
      true
    }
    accesskit::Action::Expand | accesskit::Action::Collapse => {
      // Best-effort expansion semantics:
      // - `<details>` uses its `open` attribute.
      // - `aria-expanded` uses string values.
      let expand = matches!(request.action, accesskit::Action::Expand);

      let mut dom_changed = false;
      {
        let mut index = DomIndex::build(ctx.dom);
        if let Some(node) = index.node_mut(target_node_id) {
          if node
            .tag_name()
            .is_some_and(|tag| tag.eq_ignore_ascii_case("details"))
          {
            let currently_open = node.get_attribute_ref("open").is_some();
            if expand && !currently_open {
              node.set_attribute("open", "");
              dom_changed = true;
            } else if !expand && currently_open {
              node.remove_attribute("open");
              dom_changed = true;
            }
          } else if node.get_attribute_ref("aria-expanded").is_some() {
            let next = if expand { "true" } else { "false" };
            if node.get_attribute_ref("aria-expanded") != Some(next) {
              node.set_attribute("aria-expanded", next);
              dom_changed = true;
            }
          }
        } else {
          return false;
        }
      }

      if dom_changed {
        ctx.mark_redraw();
      }
      true
    }
    accesskit::Action::Increment | accesskit::Action::Decrement => {
      let key = if matches!(request.action, accesskit::Action::Increment) {
        KeyAction::ArrowUp
      } else {
        KeyAction::ArrowDown
      };

      let (focus_changed, focus_action) = ctx
        .interaction
        .focus_node_id(ctx.dom, Some(target_node_id), true);
      ctx.push_action(focus_action);

      let changed =
        ctx
          .interaction
          .key_action_with_box_tree(ctx.dom, ctx.box_tree, key);

      if focus_changed || changed {
        ctx.mark_redraw();
      }
      true
    }
    _ => false,
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  fn find_node_id_by_id_attr(dom: &mut DomNode, id: &str) -> usize {
    let index = DomIndex::build(dom);
    *index
      .id_by_element_id
      .get(id)
      .unwrap_or_else(|| panic!("missing element #{id}"))
  }

  fn input_value(dom: &mut DomNode, node_id: usize) -> String {
    let index = DomIndex::build(dom);
    index
      .node(node_id)
      .and_then(|node| node.get_attribute_ref("value"))
      .unwrap_or("")
      .to_string()
  }

  fn has_bool_attr(dom: &mut DomNode, node_id: usize, name: &str) -> bool {
    let index = DomIndex::build(dom);
    index
      .node(node_id)
      .and_then(|node| node.get_attribute_ref(name))
      .is_some()
  }

  #[test]
  fn accesskit_focus_sets_interaction_focus() {
    let mut dom = crate::dom::parse_html(
      "<html><body><button id=\"x\">OK</button><input id=\"y\"></body></html>",
    )
    .expect("parse");
    let button_id = find_node_id_by_id_attr(&mut dom, "x");

    let mut engine = InteractionEngine::new();
    let mut needs_redraw = false;
    let mut actions = Vec::new();

    let mut ctx = ChromeDocumentContext {
      dom: &mut dom,
      interaction: &mut engine,
      box_tree: None,
      fragment_tree: None,
      scroll_state: None,
      document_url: "about:blank",
      base_url: "about:blank",
      needs_redraw: &mut needs_redraw,
      emitted_actions: Some(&mut actions),
    };

    let handled = handle_accesskit_action_request(
      &mut ctx,
      accesskit::ActionRequest {
        action: accesskit::Action::Focus,
        target: accesskit_node_id_from_fastrender(button_id),
        data: None,
      },
    );
    assert!(handled);
    assert_eq!(engine.focused_node_id(), Some(button_id));
    assert!(needs_redraw);
    assert!(
      actions
        .iter()
        .any(|a| matches!(a, InteractionAction::FocusChanged { node_id: Some(id) } if *id == button_id)),
      "expected FocusChanged action, got {actions:?}"
    );
  }

  #[test]
  fn accesskit_set_value_updates_text_input() {
    let mut dom = crate::dom::parse_html(
      "<html><body><button id=\"x\">OK</button><input id=\"y\" value=\"old\"></body></html>",
    )
    .expect("parse");
    let input_id = find_node_id_by_id_attr(&mut dom, "y");

    let mut engine = InteractionEngine::new();
    let mut needs_redraw = false;
    let mut ctx = ChromeDocumentContext {
      dom: &mut dom,
      interaction: &mut engine,
      box_tree: None,
      fragment_tree: None,
      scroll_state: None,
      document_url: "about:blank",
      base_url: "about:blank",
      needs_redraw: &mut needs_redraw,
      emitted_actions: None,
    };

    let handled = handle_accesskit_action_request(
      &mut ctx,
      accesskit::ActionRequest {
        action: accesskit::Action::SetValue,
        target: accesskit_node_id_from_fastrender(input_id),
        data: Some(accesskit::ActionData::Value("hello".into())),
      },
    );
    assert!(handled);
    assert_eq!(engine.focused_node_id(), Some(input_id));
    assert_eq!(input_value(&mut dom, input_id), "hello");
    assert!(needs_redraw);
  }

  #[test]
  fn accesskit_default_action_toggles_checkbox() {
    let mut dom = crate::dom::parse_html(
      "<html><body><input id=\"z\" type=\"checkbox\"></body></html>",
    )
    .expect("parse");
    let checkbox_id = find_node_id_by_id_attr(&mut dom, "z");

    let mut engine = InteractionEngine::new();
    let mut needs_redraw = false;
    assert!(!has_bool_attr(&mut dom, checkbox_id, "checked"));
    let mut ctx = ChromeDocumentContext {
      dom: &mut dom,
      interaction: &mut engine,
      box_tree: None,
      fragment_tree: None,
      scroll_state: None,
      document_url: "about:blank",
      base_url: "about:blank",
      needs_redraw: &mut needs_redraw,
      emitted_actions: None,
    };
    let handled = handle_accesskit_action_request(
      &mut ctx,
      accesskit::ActionRequest {
        action: accesskit::Action::Default,
        target: accesskit_node_id_from_fastrender(checkbox_id),
        data: None,
      },
    );
    assert!(handled);
    assert!(has_bool_attr(ctx.dom, checkbox_id, "checked"));
    assert!(needs_redraw);
  }
}
