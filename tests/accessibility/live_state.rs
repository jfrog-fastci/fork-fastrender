use fastrender::accessibility::{AccessibilityNode, CheckState};
use fastrender::api::FastRender;
use fastrender::dom::{enumerate_dom_ids, DomNode};
use fastrender::interaction::state::{DocumentSelectionState, FileSelection, TextEditPaintState};
use fastrender::interaction::InteractionState;
use fastrender::text::caret::CaretAffinity;
use rustc_hash::FxHashSet;
use serde_json::Value;
use std::path::PathBuf;

fn find_by_id<'a>(node: &'a AccessibilityNode, id: &str) -> Option<&'a AccessibilityNode> {
  if node.id.as_deref() == Some(id) {
    return Some(node);
  }
  for child in node.children.iter() {
    if let Some(found) = find_by_id(child, id) {
      return Some(found);
    }
  }
  None
}

fn find_dom_by_id<'a>(node: &'a DomNode, id: &str) -> Option<&'a DomNode> {
  if node
    .get_attribute_ref("id")
    .is_some_and(|value| value.eq_ignore_ascii_case(id))
  {
    return Some(node);
  }
  node.children.iter().find_map(|child| find_dom_by_id(child, id))
}

fn find_json_node<'a>(node: &'a Value, id: &str) -> Option<&'a Value> {
  if node
    .get("id")
    .and_then(|v| v.as_str())
    .is_some_and(|v| v == id)
  {
    return Some(node);
  }
  node
    .get("children")
    .and_then(|c| c.as_array())
    .into_iter()
    .flatten()
    .find_map(|child| find_json_node(child, id))
}

#[test]
fn accessibility_live_form_state_overrides_dom_attributes() {
  let mut renderer = FastRender::new().expect("renderer");
  let html = r##"
    <html>
      <body>
        <input id="text" required />
        <input id="check" type="checkbox" required checked />
        <select id="select" required>
          <option id="placeholder" value="" disabled selected>Choose</option>
          <option id="real" value="x">X</option>
        </select>
      </body>
    </html>
  "##;

  let dom = renderer.parse_html(html).expect("parse");
  let ids = enumerate_dom_ids(&dom);

  let text_node = find_dom_by_id(&dom, "text").expect("text input");
  let text_id = *ids.get(&(text_node as *const DomNode)).expect("text id");
  let check_node = find_dom_by_id(&dom, "check").expect("checkbox");
  let check_id = *ids.get(&(check_node as *const DomNode)).expect("checkbox id");
  let select_node = find_dom_by_id(&dom, "select").expect("select");
  let select_id = *ids.get(&(select_node as *const DomNode)).expect("select id");
  let real_node = find_dom_by_id(&dom, "real").expect("real option");
  let real_id = *ids.get(&(real_node as *const DomNode)).expect("real option id");

  let mut state = InteractionState::default();
  state.form_state.values.insert(text_id, "hello".to_string());
  state.form_state.checked.insert(check_id, false);
  let mut selected = FxHashSet::default();
  selected.insert(real_id);
  state.form_state.select_selected.insert(select_id, selected);

  let tree = renderer
    .accessibility_tree_with_interaction_state(&dom, 800, 600, Some(&state))
    .expect("accessibility tree");

  let text = find_by_id(&tree, "text").expect("text node");
  assert_eq!(text.value.as_deref(), Some("hello"));
  assert!(
    !text.states.invalid,
    "required text input should validate against the live value"
  );

  let check = find_by_id(&tree, "check").expect("checkbox node");
  assert_eq!(check.states.checked, Some(CheckState::False));
  assert!(
    check.states.invalid,
    "required checkbox should validate against live checked state"
  );

  let select = find_by_id(&tree, "select").expect("select node");
  assert_eq!(select.value.as_deref(), Some("X"));
  assert!(
    !select.states.invalid,
    "required select should validate against live selected option"
  );

  let placeholder = find_by_id(&tree, "placeholder").expect("placeholder option");
  assert_eq!(placeholder.states.selected, Some(false));
  let real = find_by_id(&tree, "real").expect("real option");
  assert_eq!(real.states.selected, Some(true));
}

#[test]
fn accessibility_exports_selection_debug_fields() {
  let mut renderer = FastRender::new().expect("renderer");
  let html = r##"
    <html>
      <body>
        <input id="text" type="text" value="abcdef" />
      </body>
    </html>
  "##;

  let dom = renderer.parse_html(html).expect("parse");
  let ids = enumerate_dom_ids(&dom);
  let text_node = find_dom_by_id(&dom, "text").expect("text input");
  let text_id = *ids.get(&(text_node as *const DomNode)).expect("text id");

  let mut state = InteractionState::default();
  state.focused = Some(text_id);
  state.focus_visible = true;
  state.set_focus_chain(vec![text_id]);
  state.document_selection = Some(DocumentSelectionState::All);
  state.text_edit = Some(TextEditPaintState {
    node_id: text_id,
    caret: 3,
    caret_affinity: CaretAffinity::Downstream,
    selection: Some((1, 4)),
  });

  let tree = renderer
    .accessibility_tree_with_interaction_state(&dom, 800, 600, Some(&state))
    .expect("accessibility tree");

  let json = serde_json::to_value(&tree).expect("serialize");
  assert_eq!(
    json
      .get("debug")
      .and_then(|debug| debug.get("document_has_selection"))
      .and_then(|v| v.as_bool()),
    Some(true),
    "document node should surface document selection presence"
  );

  let node = find_json_node(&json, "text").expect("text node");
  let selection = node
    .get("debug")
    .and_then(|debug| debug.get("text_selection"))
    .expect("text_selection debug");

  assert_eq!(selection.get("caret").and_then(|v| v.as_u64()), Some(3));
  assert_eq!(
    selection.get("selection_start").and_then(|v| v.as_u64()),
    Some(1)
  );
  assert_eq!(
    selection.get("selection_end").and_then(|v| v.as_u64()),
    Some(4)
  );
}

#[test]
fn accessibility_live_file_input_value_and_validation() {
  let mut renderer = FastRender::new().expect("renderer");
  let html = r##"
    <html>
      <body>
        <input id="f" type="file" required>
      </body>
    </html>
  "##;

  let dom = renderer.parse_html(html).expect("parse");
  let ids = enumerate_dom_ids(&dom);
  let file_node = find_dom_by_id(&dom, "f").expect("file input");
  let file_id = *ids.get(&(file_node as *const DomNode)).expect("file id");

  // Without state, required file inputs are invalid and do not expose a value.
  let tree = renderer
    .accessibility_tree_with_interaction_state(&dom, 800, 600, None)
    .expect("accessibility tree");
  let node = find_by_id(&tree, "f").expect("file node");
  assert_eq!(node.value.as_deref(), None);
  assert!(node.states.invalid);

  // With a live file selection, the accessibility value should mirror browser behavior and
  // required validation should pass.
  let mut state = InteractionState::default();
  state.form_state.file_inputs.insert(
    file_id,
    vec![FileSelection {
      path: PathBuf::from("/tmp/a.txt"),
      filename: "a.txt".to_string(),
      content_type: "text/plain".to_string(),
      bytes: vec![],
    }],
  );

  let tree = renderer
    .accessibility_tree_with_interaction_state(&dom, 800, 600, Some(&state))
    .expect("accessibility tree");
  let node = find_by_id(&tree, "f").expect("file node");
  assert_eq!(node.value.as_deref(), Some("C:\\fakepath\\a.txt"));
  assert!(
    !node.states.invalid,
    "required file input should validate against the live file selection"
  );

  // Multiple selected files still expose only the first filename in the value string.
  state.form_state.file_inputs.insert(
    file_id,
    vec![
      FileSelection {
        path: PathBuf::from("/tmp/a.txt"),
        filename: "a.txt".to_string(),
        content_type: "text/plain".to_string(),
        bytes: vec![],
      },
      FileSelection {
        path: PathBuf::from("/tmp/b.txt"),
        filename: "b.txt".to_string(),
        content_type: "text/plain".to_string(),
        bytes: vec![],
      },
    ],
  );

  let tree = renderer
    .accessibility_tree_with_interaction_state(&dom, 800, 600, Some(&state))
    .expect("accessibility tree");
  let node = find_by_id(&tree, "f").expect("file node");
  assert_eq!(node.value.as_deref(), Some("C:\\fakepath\\a.txt"));
}

#[test]
fn interaction_engine_document_selection_flag_is_synced() {
  use fastrender::geometry::{Point, Rect};
  use fastrender::interaction::InteractionEngine;
  use fastrender::scroll::ScrollState;
  use fastrender::style::display::FormattingContextType;
  use fastrender::style::ComputedStyle;
  use fastrender::tree::box_tree::{BoxNode, BoxTree};
  use fastrender::tree::fragment_tree::{FragmentNode, FragmentTree};
  use std::sync::Arc;

  let renderer = FastRender::new().expect("renderer");
  let html = r##"
    <html>
      <body>
        <p>hello</p>
      </body>
    </html>
  "##;
  let mut dom = renderer.parse_html(html).expect("parse");

  let box_tree = BoxTree::new(BoxNode::new_block(
    Arc::new(ComputedStyle::default()),
    FormattingContextType::Block,
    vec![],
  ));
  let fragment_tree = FragmentTree::new(FragmentNode::new_block(
    Rect::from_xywh(0.0, 0.0, 100.0, 100.0),
    vec![],
  ));
  let scroll = ScrollState::default();

  let mut engine = InteractionEngine::new();
  assert!(
    !engine
      .interaction_state()
      .document_selection
      .as_ref()
      .is_some_and(|sel| sel.has_highlight())
  );

  assert!(engine.clipboard_select_all(&mut dom));
  assert!(
    engine
      .interaction_state()
      .document_selection
      .as_ref()
      .is_some_and(|sel| sel.has_highlight())
  );

  assert!(engine.pointer_down(
    &mut dom,
    &box_tree,
    &fragment_tree,
    &scroll,
    Point::new(1.0, 1.0),
  ));
  assert!(
    !engine
      .interaction_state()
      .document_selection
      .as_ref()
      .is_some_and(|sel| sel.has_highlight())
  );
}
