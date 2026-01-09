use fastrender::css::types::StyleSheet;
use fastrender::dom;
use fastrender::dom::DomNode;
use fastrender::style::cascade::apply_styles;
use fastrender::tree::box_generation::generate_box_tree;
use fastrender::tree::box_tree::{
  BoxNode, BoxType, FormControlKind, ReplacedType, SelectControl, SelectItem,
};
use std::collections::HashMap;

fn find_select_control<'a>(node: &'a BoxNode) -> Option<&'a SelectControl> {
  if let BoxType::Replaced(replaced) = &node.box_type {
    if let ReplacedType::FormControl(control) = &replaced.replaced_type {
      if let FormControlKind::Select(select) = &control.control {
        return Some(select);
      }
    }
  }

  for child in node.children.iter() {
    if let Some(found) = find_select_control(child) {
      return Some(found);
    }
  }

  node.footnote_body.as_deref().and_then(find_select_control)
}

fn find_node_by_id<'a>(root: &'a DomNode, id: &str) -> Option<&'a DomNode> {
  let mut stack: Vec<&'a DomNode> = Vec::new();
  stack.push(root);

  while let Some(node) = stack.pop() {
    if node.get_attribute_ref("id") == Some(id) {
      return Some(node);
    }
    for child in node.children.iter().rev() {
      stack.push(child);
    }
  }

  None
}

fn collect_option_node_ids(select: &DomNode, ids: &HashMap<*const DomNode, usize>) -> Vec<usize> {
  let mut out = Vec::new();
  let mut stack: Vec<&DomNode> = Vec::new();
  stack.push(select);

  while let Some(node) = stack.pop() {
    if node
      .tag_name()
      .is_some_and(|tag| tag.eq_ignore_ascii_case("option"))
    {
      let node_id = ids
        .get(&(node as *const DomNode))
        .copied()
        .expect("<option> node id should be present");
      out.push(node_id);
      // `<option>` nodes cannot contain other `<option>` nodes in well-formed HTML; mirror the
      // select flattener by not traversing children once matched.
      continue;
    }

    for child in node.children.iter().rev() {
      stack.push(child);
    }
  }

  out
}

#[test]
fn select_control_option_items_track_dom_node_ids() {
  let html = "<html><body><select id=\"s\">\
    <option id=\"o1\">One</option>\
    <optgroup id=\"g1\" label=\"Group\" disabled>\
      <option id=\"o2\">Two</option>\
      <option id=\"o3\" disabled>Three</option>\
    </optgroup>\
    <option id=\"o4\" disabled>Four</option>\
  </select></body></html>";

  let dom = dom::parse_html(html).expect("parse html");
  let dom_ids = dom::enumerate_dom_ids(&dom);
  let select_node = find_node_by_id(&dom, "s").expect("expected <select id=s>");
  let expected_option_ids = collect_option_node_ids(select_node, &dom_ids);

  let styled = apply_styles(&dom, &StyleSheet::new());
  let box_tree = generate_box_tree(&styled).expect("box tree");

  let select = find_select_control(&box_tree.root).expect("select control");
  let actual_option_ids: Vec<usize> = select
    .items
    .iter()
    .filter_map(|item| match item {
      SelectItem::Option { node_id, .. } => Some(*node_id),
      _ => None,
    })
    .collect();

  assert_eq!(
    actual_option_ids, expected_option_ids,
    "SelectControl option rows should map back to DOM preorder ids"
  );
}
