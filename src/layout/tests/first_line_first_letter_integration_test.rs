use crate::css::parser::parse_stylesheet;
use crate::dom;
use crate::layout::constraints::LayoutConstraints;
use crate::layout::contexts::inline::InlineFormattingContext;
use crate::style::cascade::apply_styles;
use crate::tree::box_generation::generate_box_tree;
use crate::tree::box_tree::BoxNode;
use crate::FormattingContext;
use crate::FragmentContent;
use crate::FragmentNode;
use crate::Rgba;

fn find_first_styled_node_id(node: &crate::style::cascade::StyledNode, tag: &str) -> Option<usize> {
  if node
    .node
    .tag_name()
    .is_some_and(|name| name.eq_ignore_ascii_case(tag))
  {
    return Some(node.node_id);
  }
  node
    .children
    .iter()
    .find_map(|child| find_first_styled_node_id(child, tag))
}

fn find_box_for_styled_node_id<'a>(node: &'a BoxNode, styled_node_id: usize) -> Option<&'a BoxNode> {
  if node.styled_node_id == Some(styled_node_id) {
    return Some(node);
  }
  node
    .children
    .iter()
    .find_map(|child| find_box_for_styled_node_id(child, styled_node_id))
}

fn collect_texts<'a>(fragment: &'a FragmentNode, out: &mut Vec<(&'a str, Rgba)>) {
  if let FragmentContent::Text {
    text, is_marker, ..
  } = &fragment.content
  {
    if !is_marker {
      let color = fragment
        .style
        .as_ref()
        .map(|s| s.color)
        .unwrap_or_else(|| Rgba::BLACK);
      out.push((text.as_ref(), color));
    }
  }
  for child in fragment.children.iter() {
    collect_texts(child, out);
  }
}

#[test]
fn first_line_and_first_letter_styles_flow_through_pipeline() {
  let html = "<p>hello world</p>";
  let css = "p::first-letter { color: rgb(200, 0, 0); } p::first-line { color: rgb(0, 0, 255); }";

  let dom = dom::parse_html(html).expect("parse html");
  let stylesheet = parse_stylesheet(css).expect("parse stylesheet");
  let styled = apply_styles(&dom, &stylesheet);
  let box_tree = generate_box_tree(&styled).expect("box tree");

  let p_node_id = find_first_styled_node_id(&styled, "p").expect("find <p> in styled tree");
  let paragraph =
    find_box_for_styled_node_id(&box_tree.root, p_node_id).expect("paragraph box");

  let ifc = InlineFormattingContext::new();
  let fragment = ifc
    .layout(paragraph, &LayoutConstraints::definite_width(200.0))
    .expect("inline layout");

  let mut texts = Vec::new();
  collect_texts(&fragment, &mut texts);

  assert!(
    texts
      .iter()
      .any(|(text, color)| text.starts_with('h') && *color == Rgba::rgb(200, 0, 0)),
    "first-letter fragment should carry the first-letter color"
  );
  assert!(
    texts
      .iter()
      .any(|(text, color)| text.contains("ello") && *color == Rgba::rgb(0, 0, 255)),
    "remaining first-line text should use the first-line color"
  );
}
