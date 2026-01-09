use fastrender::css::types::StyleSheet;
use fastrender::dom;
use fastrender::style::cascade::apply_styles;
use fastrender::style::display::Display;
use fastrender::tree::box_generation::generate_box_tree_with_anonymous_fixup;
use fastrender::tree::box_tree::BoxNode;

fn node_has_class(node: &BoxNode, class: &str) -> bool {
  node
    .debug_info
    .as_ref()
    .is_some_and(|info| info.classes.iter().any(|c| c == class))
}

fn find_first_by_class<'a>(node: &'a BoxNode, class: &str) -> Option<&'a BoxNode> {
  if node_has_class(node, class) {
    return Some(node);
  }
  node
    .children
    .iter()
    .find_map(|child| find_first_by_class(child, class))
}

#[test]
fn flex_items_are_blockified() {
  let html = r#"<div style="display:flex"><span class="item">Item</span></div>"#;
  let dom: dom::DomNode = dom::parse_html(html).expect("parse html");
  let styled = apply_styles(&dom, &StyleSheet::new());
  let box_tree = generate_box_tree_with_anonymous_fixup(&styled).expect("box tree");

  let item = find_first_by_class(&box_tree.root, "item").expect("item present");

  assert_eq!(
    item.style.display,
    Display::Block,
    "flex/grid items should be blockified (used display becomes block-level)"
  );
  assert!(item.box_type.is_block_level());
}

#[test]
fn grid_items_are_blockified() {
  let html = r#"<div style="display:grid"><span class="item">Item</span></div>"#;
  let dom: dom::DomNode = dom::parse_html(html).expect("parse html");
  let styled = apply_styles(&dom, &StyleSheet::new());
  let box_tree = generate_box_tree_with_anonymous_fixup(&styled).expect("box tree");

  let item = find_first_by_class(&box_tree.root, "item").expect("item present");

  assert_eq!(item.style.display, Display::Block);
  assert!(item.box_type.is_block_level());
}

#[test]
fn display_contents_descendants_are_blockified_as_items() {
  let html =
    r#"<div style="display:flex"><div style="display:contents"><span class="item">Item</span></div></div>"#;
  let dom: dom::DomNode = dom::parse_html(html).expect("parse html");
  let styled = apply_styles(&dom, &StyleSheet::new());
  let box_tree = generate_box_tree_with_anonymous_fixup(&styled).expect("box tree");

  let item = find_first_by_class(&box_tree.root, "item").expect("item present");

  assert_eq!(item.style.display, Display::Block);
  assert!(item.box_type.is_block_level());
}

#[test]
fn flex_replaced_items_are_blockified() {
  let html = r#"<div style="display:flex"><img class="item" src="example.png"></div>"#;
  let dom: dom::DomNode = dom::parse_html(html).expect("parse html");
  let styled = apply_styles(&dom, &StyleSheet::new());
  let box_tree = generate_box_tree_with_anonymous_fixup(&styled).expect("box tree");

  let item = find_first_by_class(&box_tree.root, "item").expect("item present");

  assert!(item.box_type.is_replaced(), "expected <img> to create a replaced box");
  assert_eq!(
    item.style.display,
    Display::Block,
    "replaced flex/grid items should be blockified (used display becomes block-level)"
  );
}
