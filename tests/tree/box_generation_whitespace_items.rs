use fastrender::css::types::StyleSheet;
use fastrender::dom;
use fastrender::style::cascade::apply_styles;
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
fn flex_container_ignores_collapsible_whitespace_text_nodes() {
  let html = r#"
    <div class="flex" style="display:flex">
      <div class="a"></div>
      <div class="b"></div>
    </div>
  "#;
  let dom: fastrender::dom::DomNode = dom::parse_html(html).expect("parse html");
  let styled = apply_styles(&dom, &StyleSheet::new());
  let box_tree = generate_box_tree_with_anonymous_fixup(&styled).expect("box tree");

  let flex = find_first_by_class(&box_tree.root, "flex").expect("flex node present");

  assert_eq!(
    flex.children.len(),
    2,
    "collapsible whitespace between flex items should not generate anonymous flex items"
  );
  assert!(node_has_class(&flex.children[0], "a"));
  assert!(node_has_class(&flex.children[1], "b"));
}

#[test]
fn grid_container_ignores_collapsible_whitespace_text_nodes() {
  let html = r#"
    <div class="grid" style="display:grid">
      <div class="a"></div>
      <div class="b"></div>
    </div>
  "#;
  let dom: fastrender::dom::DomNode = dom::parse_html(html).expect("parse html");
  let styled = apply_styles(&dom, &StyleSheet::new());
  let box_tree = generate_box_tree_with_anonymous_fixup(&styled).expect("box tree");

  let grid = find_first_by_class(&box_tree.root, "grid").expect("grid node present");

  assert_eq!(
    grid.children.len(),
    2,
    "collapsible whitespace between grid items should not generate anonymous grid items"
  );
  assert!(node_has_class(&grid.children[0], "a"));
  assert!(node_has_class(&grid.children[1], "b"));
}
