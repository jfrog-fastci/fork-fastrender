#![cfg(feature = "browser_ui")]

use crate::api::BrowserTab;
use crate::dom::DomNode;
use crate::dom2;
use crate::interaction::dom_index::DomIndex;
use crate::interaction::{InteractionAction, InteractionEngine, KeyAction};
use crate::scroll::ScrollState;
use crate::tree::box_tree::BoxTree;
use crate::tree::fragment_tree::FragmentTree;
use crate::ui::{dom_node_id_for_current_page_action, page_accesskit_ids, TabId};

/// Shared context for routing AccessKit [`accesskit::ActionRequest`]s into FastRender's interaction
/// engine.
///
/// The windowed browser UI can use this to translate screen reader requests (e.g. "press" on a
/// button) into the same internal operations used by pointer/keyboard input.
pub struct ChromeDocumentContext<'a> {
  pub dom: &'a mut DomNode,
  pub interaction: &'a mut InteractionEngine,

  /// Optional JS-backed tab: when present, focus changes routed from AccessKit will dispatch trusted
  /// DOM focus events (`blur`/`focus` + bubbling `focusin`/`focusout`) so scripts observe the same
  /// flow as user interactions.
  pub js_tab: Option<&'a mut BrowserTab>,

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

  fn dispatch_focus_change_to_js(
    &mut self,
    prev: Option<usize>,
    prev_element_id: Option<&str>,
    next: Option<usize>,
    next_element_id: Option<&str>,
  ) {
    if prev == next {
      return;
    }
    let Some(js_tab) = self.js_tab.as_mut() else {
      return;
    };

    fn js_dom_node_for_preorder_id(
      js_tab: &mut BrowserTab,
      preorder_id: usize,
      element_id: Option<&str>,
    ) -> Option<dom2::NodeId> {
      // Prefer mapping renderer preorder ids back into stable dom2 NodeIds. Renderer preorder ids
      // can shift under DOM mutations, and `dom2::NodeId` allocation order does not match traversal
      // order, so indexing into the dom2 node list is unsafe.
      if let Some(mapped) = js_tab.dom2_node_for_renderer_preorder(preorder_id) {
        // If the caller also supplies an element id, treat it as a stability check: preorder ids can
        // become stale across dom2 DOM mutations, but the element's `id=` attribute remains stable.
        // If the mapped node does not match the expected id, fall back to a fresh `getElementById`
        // lookup.
        if let Some(id) = element_id {
          let dom = js_tab.dom();
          let mapped_id = dom.get_attribute(mapped, "id").ok().flatten();
          if mapped_id != Some(id) {
            return dom.get_element_by_id(id);
          }
        }
        return Some(mapped);
      }

      element_id.and_then(|id| js_tab.dom().get_element_by_id(id))
    }

    let prev_js_node_id =
      prev.and_then(|id| js_dom_node_for_preorder_id(js_tab, id, prev_element_id));
    let next_js_node_id =
      next.and_then(|id| js_dom_node_for_preorder_id(js_tab, id, next_element_id));

    // Dispatch `focusin`/`focusout` first so bubbled listeners observe a deterministic sequence.
    // Keep non-bubbling `blur`/`focus` as the final notifications.
    if let Some(prev_js_node_id) = prev_js_node_id {
      let _ = js_tab.dispatch_focusout_event(prev_js_node_id);
    }
    if let Some(next_js_node_id) = next_js_node_id {
      let _ = js_tab.dispatch_focusin_event(next_js_node_id);
    }
    if let Some(prev_js_node_id) = prev_js_node_id {
      let _ = js_tab.dispatch_blur_event(prev_js_node_id);
    }
    if let Some(next_js_node_id) = next_js_node_id {
      let _ = js_tab.dispatch_focus_event(next_js_node_id);
    }
  }

  fn focus_node_id(&mut self, node_id: usize) -> (bool, InteractionAction) {
    let prev = self.interaction.focused_node_id();
    // Cache the focused element id before the focus mutation so we can dispatch JS focus events
    // without doing an additional DOM preorder walk just to read `id=`.
    let prev_element_id = self.interaction.focused_element_id().map(|id| id.to_string());
    let (changed, action) = self
      .interaction
      .focus_node_id(self.dom, Some(node_id), true);
    let next = self.interaction.focused_node_id();
    let next_element_id = self.interaction.focused_element_id().map(|id| id.to_string());
    self.dispatch_focus_change_to_js(
      prev,
      prev_element_id.as_deref(),
      next,
      next_element_id.as_deref(),
    );
    (changed, action)
  }
}

/// Decode an AccessKit [`accesskit::NodeId`] into a FastRender DOM pre-order node id.
///
/// Decoding precedence:
///
/// 1. **Canonical:** try the `(tab_id, document_generation, dom_node_id)` encoding produced by
///    [`crate::ui::encode_page_node_id`]. This allows filtering stale action requests across
///    navigations (generation mismatch).
/// 2. **Compatibility:** fall back to the tag-bit encoding in [`crate::ui::page_accesskit_ids`]
///    (`(tab_id, dom_node_id)`), which can still filter by tab but cannot filter stale navigations
///    (no generation is encoded).
///
/// In both cases, egui/chrome `NodeId`s are rejected and requests targeting other tabs are ignored.
pub fn fastrender_node_id_from_accesskit(
  node_id: accesskit::NodeId,
  current_tab_id: TabId,
  current_document_generation: u32,
) -> Option<usize> {
  if let Some(dom_node_id) =
    dom_node_id_for_current_page_action(node_id, current_tab_id, current_document_generation)
  {
    return Some(dom_node_id);
  }

  // Back-compat for the tag-bit encoding in `ui::page_accesskit_ids`. This scheme does not carry
  // a document generation, so callers cannot filter stale action requests across navigations when
  // using it. Prefer `encode_page_node_id` for real page subtree integration.
  let (tab_id, dom_node_id) = page_accesskit_ids::decode_page_node_id(node_id)?;
  if tab_id != current_tab_id {
    return None;
  }
  Some(dom_node_id)
}

fn text_control_len_chars(dom: &mut DomNode, node_id: usize) -> Option<usize> {
  let node = crate::dom::find_node_mut_by_preorder_id(dom, node_id)?;
  let tag = node.tag_name()?;
  if tag.eq_ignore_ascii_case("textarea") {
    Some(crate::dom::textarea_current_value(node).chars().count())
  } else if tag.eq_ignore_ascii_case("input") {
    Some(
      node
        .get_attribute_ref("value")
        .unwrap_or("")
        .chars()
        .count(),
    )
  } else {
    None
  }
}

/// Route a single AccessKit [`accesskit::ActionRequest`] into FastRender's [`InteractionEngine`].
///
/// Returns `true` when the request was recognized and dispatched.
pub fn handle_accesskit_action_request(
  ctx: &mut ChromeDocumentContext<'_>,
  current_tab_id: TabId,
  current_document_generation: u32,
  request: accesskit::ActionRequest,
) -> bool {
  let Some(target_node_id) =
    fastrender_node_id_from_accesskit(request.target, current_tab_id, current_document_generation)
  else {
    return false;
  };

  match request.action {
    accesskit::Action::Focus => {
      let (changed, action) = ctx.focus_node_id(target_node_id);
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
      let (focus_changed, focus_action) = ctx.focus_node_id(target_node_id);
      ctx.push_action(focus_action);

      let (changed, action) =
        ctx
          .interaction
          .key_activate(ctx.dom, KeyAction::Enter, ctx.document_url, ctx.base_url);
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

      let (focus_changed, focus_action) = ctx.focus_node_id(target_node_id);
      ctx.push_action(focus_action);

      let changed = ctx
        .interaction
        .set_text_control_value(ctx.dom, target_node_id, &value);

      if focus_changed || changed {
        ctx.mark_redraw();
      }
      true
    }
    accesskit::Action::SetTextSelection => {
      let Some(data) = request.data else {
        return false;
      };
      let selection = match data {
        accesskit::ActionData::SetTextSelection(selection) => selection,
        _ => return false,
      };

      // AccessKit positions should refer to the same text control node.
      if selection.anchor.node != request.target || selection.focus.node != request.target {
        return false;
      }

      let (focus_changed, focus_action) = ctx.focus_node_id(target_node_id);
      ctx.push_action(focus_action);

      let before = ctx.interaction.interaction_state().text_edit;

      let max_len = text_control_len_chars(ctx.dom, target_node_id).unwrap_or(0);
      let anchor = selection.anchor.character_index.min(max_len);
      let focus = selection.focus.character_index.min(max_len);
      if anchor == focus {
        ctx
          .interaction
          .set_text_selection_caret(target_node_id, anchor);
      } else {
        // Preserve selection direction (caret should be at the requested focus offset).
        ctx
          .interaction
          .set_text_selection_range(target_node_id, anchor, focus);
      }

      let changed = before != ctx.interaction.interaction_state().text_edit;
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

      let (focus_changed, focus_action) = ctx.focus_node_id(target_node_id);
      ctx.push_action(focus_action);

      let changed = ctx
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
  use crate::api::{RenderOptions, VmJsBrowserTabExecutor};

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

  const TEST_TAB_ID: TabId = TabId(1);
  const TEST_DOCUMENT_GENERATION: u32 = 1;

  fn page_node_id(dom_node_id: usize) -> accesskit::NodeId {
    crate::ui::encode_page_node_id(TEST_TAB_ID, TEST_DOCUMENT_GENERATION, dom_node_id)
  }

  #[test]
  fn accesskit_focus_sets_interaction_focus() {
    let tab_id = TEST_TAB_ID;
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
      js_tab: None,
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
      tab_id,
      TEST_DOCUMENT_GENERATION,
      accesskit::ActionRequest {
        action: accesskit::Action::Focus,
        target: page_node_id(button_id),
        data: None,
      },
    );
    assert!(handled);
    assert_eq!(engine.focused_node_id(), Some(button_id));
    assert!(needs_redraw);
    assert!(
      actions.iter().any(
        |a| matches!(a, InteractionAction::FocusChanged { node_id: Some(id) } if *id == button_id)
      ),
      "expected FocusChanged action, got {actions:?}"
    );
  }

  #[test]
  fn accesskit_set_value_updates_text_input() {
    let tab_id = TEST_TAB_ID;
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
      js_tab: None,
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
      tab_id,
      TEST_DOCUMENT_GENERATION,
      accesskit::ActionRequest {
        action: accesskit::Action::SetValue,
        target: page_node_id(input_id),
        data: Some(accesskit::ActionData::Value("hello".into())),
      },
    );
    assert!(handled);
    assert_eq!(engine.focused_node_id(), Some(input_id));
    assert_eq!(input_value(&mut dom, input_id), "hello");
    assert!(needs_redraw);
  }

  #[test]
  fn accesskit_set_text_selection_moves_caret_and_sets_range() {
    let tab_id = TEST_TAB_ID;
    let mut dom =
      crate::dom::parse_html("<html><body><input id=\"y\" value=\"abcdef\"></body></html>")
        .expect("parse");
    let input_id = find_node_id_by_id_attr(&mut dom, "y");
    let target = page_node_id(input_id);

    let mut engine = InteractionEngine::new();
    let mut needs_redraw = false;

    let caret_req = accesskit::ActionRequest {
      action: accesskit::Action::SetTextSelection,
      target,
      data: Some(accesskit::ActionData::SetTextSelection(
        accesskit::TextSelection {
          anchor: accesskit::TextPosition {
            node: target,
            character_index: 3,
          },
          focus: accesskit::TextPosition {
            node: target,
            character_index: 3,
          },
        },
      )),
    };

    {
      let mut ctx = ChromeDocumentContext {
        dom: &mut dom,
        interaction: &mut engine,
        js_tab: None,
        box_tree: None,
        fragment_tree: None,
        scroll_state: None,
        document_url: "about:blank",
        base_url: "about:blank",
        needs_redraw: &mut needs_redraw,
        emitted_actions: None,
      };
      assert!(handle_accesskit_action_request(
        &mut ctx,
        tab_id,
        TEST_DOCUMENT_GENERATION,
        caret_req
      ));
    }
    let edit = engine
      .interaction_state()
      .text_edit
      .expect("expected edit state");
    assert_eq!(edit.node_id, input_id);
    assert_eq!(edit.caret, 3);
    assert_eq!(edit.selection, None);
    assert!(needs_redraw);

    needs_redraw = false;
    let range_req = accesskit::ActionRequest {
      action: accesskit::Action::SetTextSelection,
      target,
      data: Some(accesskit::ActionData::SetTextSelection(
        accesskit::TextSelection {
          anchor: accesskit::TextPosition {
            node: target,
            character_index: 1,
          },
          focus: accesskit::TextPosition {
            node: target,
            character_index: 4,
          },
        },
      )),
    };

    {
      let mut ctx = ChromeDocumentContext {
        dom: &mut dom,
        interaction: &mut engine,
        js_tab: None,
        box_tree: None,
        fragment_tree: None,
        scroll_state: None,
        document_url: "about:blank",
        base_url: "about:blank",
        needs_redraw: &mut needs_redraw,
        emitted_actions: None,
      };
      assert!(handle_accesskit_action_request(
        &mut ctx,
        tab_id,
        TEST_DOCUMENT_GENERATION,
        range_req
      ));
    }
    let edit = engine
      .interaction_state()
      .text_edit
      .expect("expected edit state");
    assert_eq!(edit.node_id, input_id);
    assert_eq!(edit.caret, 4);
    assert_eq!(edit.selection, Some((1, 4)));
    assert!(needs_redraw);

    // Reverse selection (anchor after focus) should preserve focus as the caret.
    needs_redraw = false;
    let reverse_req = accesskit::ActionRequest {
      action: accesskit::Action::SetTextSelection,
      target,
      data: Some(accesskit::ActionData::SetTextSelection(
        accesskit::TextSelection {
          anchor: accesskit::TextPosition {
            node: target,
            character_index: 4,
          },
          focus: accesskit::TextPosition {
            node: target,
            character_index: 1,
          },
        },
      )),
    };

    {
      let mut ctx = ChromeDocumentContext {
        dom: &mut dom,
        interaction: &mut engine,
        js_tab: None,
        box_tree: None,
        fragment_tree: None,
        scroll_state: None,
        document_url: "about:blank",
        base_url: "about:blank",
        needs_redraw: &mut needs_redraw,
        emitted_actions: None,
      };
      assert!(handle_accesskit_action_request(
        &mut ctx,
        tab_id,
        TEST_DOCUMENT_GENERATION,
        reverse_req
      ));
    }
    let edit = engine
      .interaction_state()
      .text_edit
      .expect("expected edit state");
    assert_eq!(edit.node_id, input_id);
    assert_eq!(edit.caret, 1);
    assert_eq!(edit.selection, Some((1, 4)));
    assert!(needs_redraw);
  }

  #[test]
  fn accesskit_default_action_toggles_checkbox() {
    let tab_id = TEST_TAB_ID;
    let mut dom =
      crate::dom::parse_html("<html><body><input id=\"z\" type=\"checkbox\"></body></html>")
        .expect("parse");
    let checkbox_id = find_node_id_by_id_attr(&mut dom, "z");

    let mut engine = InteractionEngine::new();
    let mut needs_redraw = false;
    assert!(!has_bool_attr(&mut dom, checkbox_id, "checked"));
    let mut ctx = ChromeDocumentContext {
      dom: &mut dom,
      interaction: &mut engine,
      js_tab: None,
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
      tab_id,
      TEST_DOCUMENT_GENERATION,
      accesskit::ActionRequest {
        action: accesskit::Action::Default,
        target: page_node_id(checkbox_id),
        data: None,
      },
    );
    assert!(handled);
    assert!(has_bool_attr(ctx.dom, checkbox_id, "checked"));
    assert!(needs_redraw);
  }

  #[test]
  fn accesskit_focus_dispatches_focus_blur_events_into_js() -> crate::Result<()> {
    let tab_id = TEST_TAB_ID;
    let _lock = crate::testing::global_test_lock();

    let html = r#"<!doctype html>
<input id="a">
<input id="b">
<script>
  var a = document.getElementById("a");
  var b = document.getElementById("b");

  a.onfocus = function () { a.setAttribute("data-focused", "1"); };
  a.onblur = function () { a.setAttribute("data-blurred", "1"); };

  b.onfocus = function () { b.setAttribute("data-focused", "1"); };
  b.onblur = function () { b.setAttribute("data-blurred", "1"); };

  document.body.addEventListener("focusin", function (ev) {
    var log = document.body.getAttribute("data-log") || "";
    document.body.setAttribute("data-log", log + "focusin:" + ev.target.id + ";");
  });
  document.body.addEventListener("focusout", function (ev) {
    var log = document.body.getAttribute("data-log") || "";
    document.body.setAttribute("data-log", log + "focusout:" + ev.target.id + ";");
  });

  // `focus`/`blur` should not bubble.
  document.body.addEventListener("focus", function () {
    document.body.setAttribute("data-focus-bubbled", "1");
  });
  document.body.addEventListener("blur", function () {
    document.body.setAttribute("data-blur-bubbled", "1");
  });
</script>
"#;

    let executor = VmJsBrowserTabExecutor::new();
    let mut tab =
      BrowserTab::from_html(html, RenderOptions::new().with_viewport(64, 64), executor)?;

    let mut dom = crate::dom::parse_html(html)?;
    let a_id = find_node_id_by_id_attr(&mut dom, "a");
    let b_id = find_node_id_by_id_attr(&mut dom, "b");

    let mut engine = InteractionEngine::new();
    let mut needs_redraw = false;

    {
      let mut ctx = ChromeDocumentContext {
        dom: &mut dom,
        interaction: &mut engine,
        js_tab: Some(&mut tab),
        box_tree: None,
        fragment_tree: None,
        scroll_state: None,
        document_url: "about:blank",
        base_url: "about:blank",
        needs_redraw: &mut needs_redraw,
        emitted_actions: None,
      };

      assert!(handle_accesskit_action_request(
        &mut ctx,
        tab_id,
        TEST_DOCUMENT_GENERATION,
        accesskit::ActionRequest {
          action: accesskit::Action::Focus,
          target: page_node_id(a_id),
          data: None,
        },
      ));
      assert!(handle_accesskit_action_request(
        &mut ctx,
        tab_id,
        TEST_DOCUMENT_GENERATION,
        accesskit::ActionRequest {
          action: accesskit::Action::Focus,
          target: page_node_id(b_id),
          data: None,
        },
      ));
    }

    let dom2 = tab.dom();
    let a = dom2
      .get_element_by_id("a")
      .expect("expected <input id=a> in dom2");
    let b = dom2
      .get_element_by_id("b")
      .expect("expected <input id=b> in dom2");
    let body = dom2.body().expect("expected <body> element");

    assert_eq!(dom2.get_attribute(a, "data-focused").unwrap(), Some("1"));
    assert_eq!(dom2.get_attribute(a, "data-blurred").unwrap(), Some("1"));
    assert_eq!(dom2.get_attribute(b, "data-focused").unwrap(), Some("1"));
    assert_eq!(
      dom2.get_attribute(b, "data-blurred").unwrap(),
      None,
      "expected b to remain focused after focusing it"
    );

    assert_eq!(
      dom2.get_attribute(body, "data-log").unwrap(),
      Some("focusin:a;focusout:a;focusin:b;"),
      "expected bubbling focusin/focusout events to be observed on <body>"
    );
    assert_eq!(
      dom2.get_attribute(body, "data-focus-bubbled").unwrap(),
      None,
      "expected non-bubbling focus event to not reach <body>"
    );
    assert_eq!(
      dom2.get_attribute(body, "data-blur-bubbled").unwrap(),
      None,
      "expected non-bubbling blur event to not reach <body>"
    );

    Ok(())
  }

  #[test]
  fn accesskit_decoding_ignores_other_tabs_and_generations() {
    // Use a small dom id (1) to ensure we don't accidentally rely on "big values" to avoid
    // collisions; the namespacing scheme must handle this safely.
    let dom_node_id = 1usize;

    let tab_id = TEST_TAB_ID;
    let gen = TEST_DOCUMENT_GENERATION;
    let node_id = crate::ui::encode_page_node_id(tab_id, gen, dom_node_id);

    assert_eq!(
      fastrender_node_id_from_accesskit(node_id, tab_id, gen),
      Some(dom_node_id)
    );
    assert_eq!(
      fastrender_node_id_from_accesskit(node_id, TabId(2), gen),
      None,
      "expected ids for other tabs to be ignored"
    );
    assert_eq!(
      fastrender_node_id_from_accesskit(node_id, tab_id, gen + 1),
      None,
      "expected stale generation ids to be ignored"
    );
  }

  #[test]
  fn accesskit_decoding_accepts_tagged_page_ids_for_current_tab() {
    let tab_id = TEST_TAB_ID;
    let gen = TEST_DOCUMENT_GENERATION;
    let dom_node_id = 123usize;

    let tagged = page_accesskit_ids::page_node_id(tab_id, dom_node_id);
    assert_eq!(
      fastrender_node_id_from_accesskit(tagged, tab_id, gen),
      Some(dom_node_id)
    );

    let other_tab = page_accesskit_ids::page_node_id(TabId(2), dom_node_id);
    assert_eq!(
      fastrender_node_id_from_accesskit(other_tab, tab_id, gen),
      None,
      "expected tagged ids for other tabs to be ignored"
    );
  }

  #[test]
  fn action_routing_ignores_requests_for_other_tabs_and_generations() {
    let tab_id = TEST_TAB_ID;
    let gen = TEST_DOCUMENT_GENERATION;

    let mut dom = crate::dom::parse_html("<html><body><button id=\"x\">OK</button></body></html>")
      .expect("parse");
    let button_id = find_node_id_by_id_attr(&mut dom, "x");

    let mut engine = InteractionEngine::new();
    let mut needs_redraw = false;
    let mut ctx = ChromeDocumentContext {
      dom: &mut dom,
      interaction: &mut engine,
      js_tab: None,
      box_tree: None,
      fragment_tree: None,
      scroll_state: None,
      document_url: "about:blank",
      base_url: "about:blank",
      needs_redraw: &mut needs_redraw,
      emitted_actions: None,
    };

    let other_tab_target = crate::ui::encode_page_node_id(TabId(2), gen, button_id);
    let handled_other_tab = handle_accesskit_action_request(
      &mut ctx,
      tab_id,
      gen,
      accesskit::ActionRequest {
        action: accesskit::Action::Focus,
        target: other_tab_target,
        data: None,
      },
    );
    assert!(
      !handled_other_tab,
      "expected action requests for other tabs to be ignored"
    );
    assert_eq!(
      engine.focused_node_id(),
      None,
      "expected focus to remain unchanged"
    );
    assert!(
      !needs_redraw,
      "expected unrelated-tab action routing to not request redraw"
    );

    needs_redraw = false;
    let stale_target = crate::ui::encode_page_node_id(tab_id, gen - 1, button_id);
    let handled_stale = handle_accesskit_action_request(
      &mut ctx,
      tab_id,
      gen,
      accesskit::ActionRequest {
        action: accesskit::Action::Focus,
        target: stale_target,
        data: None,
      },
    );
    assert!(
      !handled_stale,
      "expected action requests for stale document generations to be ignored"
    );
    assert_eq!(
      engine.focused_node_id(),
      None,
      "expected focus to remain unchanged"
    );
    assert!(
      !needs_redraw,
      "expected stale-generation action routing to not request redraw"
    );
  }
}
