use crate::api::FastRender;
use crate::dom::DomNodeType;
use crate::style::cascade::StyledNode;
use crate::style::media::MediaType;
use crate::tree::box_tree::BoxNode;
use crate::tree::fragment_tree::{FragmentContent, FragmentNode};
use crate::{Point, Rect};

fn styled_node_tag_name(node: &StyledNode) -> Option<&str> {
  match &node.node.node_type {
    DomNodeType::Element { tag_name, .. } => Some(tag_name),
    DomNodeType::Slot { .. } => Some("slot"),
    _ => None,
  }
}

fn styled_node_attr_value<'a>(node: &'a StyledNode, name: &str) -> Option<&'a str> {
  let attrs = match &node.node.node_type {
    DomNodeType::Element { attributes, .. } | DomNodeType::Slot { attributes, .. } => attributes,
    _ => return None,
  };

  attrs
    .iter()
    .find(|(k, _)| k.eq_ignore_ascii_case(name))
    .map(|(_, v)| v.as_str())
}

fn find_styled_node<'a>(
  root: &'a StyledNode,
  predicate: &impl Fn(&StyledNode) -> bool,
) -> Option<&'a StyledNode> {
  if predicate(root) {
    return Some(root);
  }
  for child in root.children.iter() {
    if let Some(found) = find_styled_node(child, predicate) {
      return Some(found);
    }
  }
  None
}

fn find_box_node_by_styled_id<'a>(root: &'a BoxNode, styled_node_id: usize) -> Option<&'a BoxNode> {
  if root.generated_pseudo.is_none() && root.styled_node_id == Some(styled_node_id) {
    return Some(root);
  }
  for child in root.children.iter() {
    if let Some(found) = find_box_node_by_styled_id(child, styled_node_id) {
      return Some(found);
    }
  }
  if let Some(body) = root.footnote_body.as_deref() {
    if let Some(found) = find_box_node_by_styled_id(body, styled_node_id) {
      return Some(found);
    }
  }
  None
}

fn fragment_box_id(fragment: &FragmentNode) -> Option<usize> {
  match fragment.content {
    FragmentContent::Block { box_id }
    | FragmentContent::Inline { box_id, .. }
    | FragmentContent::Text { box_id, .. }
    | FragmentContent::Replaced { box_id, .. } => box_id,
    FragmentContent::Line { .. }
    | FragmentContent::RunningAnchor { .. }
    | FragmentContent::FootnoteAnchor { .. } => None,
  }
}

fn find_fragment_abs_bounds_by_box_id(
  root: &FragmentNode,
  box_id: usize,
  abs_origin: Point,
) -> Option<Rect> {
  let current_origin = abs_origin.translate(Point::new(root.bounds.x(), root.bounds.y()));
  if fragment_box_id(root) == Some(box_id) {
    return Some(Rect::from_xywh(
      current_origin.x,
      current_origin.y,
      root.bounds.width(),
      root.bounds.height(),
    ));
  }

  for child in root.children.iter() {
    if let Some(found) = find_fragment_abs_bounds_by_box_id(child, box_id, current_origin) {
      return Some(found);
    }
  }

  None
}

#[test]
fn interactive_example_height_regression() {
  // Regression test for MDN docs pages which style the interactive-example placeholder with a type
  // selector on a custom element:
  //
  //   .content-section interactive-example { display:block; height:375px; }
  //
  // FastRender must:
  // - match custom-element type selectors (tag names containing '-')
  // - honor specified `height` on an empty in-flow block element
  let html = r#"
    <html>
      <head>
        <style>
          body { margin: 0; }
          p { margin: 0; }
          .content-section interactive-example { display: block; height: 100px; }
        </style>
      </head>
      <body>
        <section class="content-section"><interactive-example></interactive-example><p id="after">After</p></section>
      </body>
    </html>
  "#;

  let mut renderer = FastRender::new().expect("renderer");
  let dom = renderer.parse_html(html).expect("parse HTML");

  let layout = renderer
    .layout_document_for_media_intermediates(&dom, 320, 240, MediaType::Screen)
    .expect("layout intermediates");

  let interactive_styled = find_styled_node(&layout.styled_tree, &|node| {
    styled_node_tag_name(node) == Some("interactive-example")
  })
  .expect("styled node for <interactive-example>");

  let after_styled = find_styled_node(&layout.styled_tree, &|node| {
    styled_node_tag_name(node) == Some("p") && styled_node_attr_value(node, "id") == Some("after")
  })
  .expect("styled node for <p id=after>");

  let interactive_box =
    find_box_node_by_styled_id(&layout.box_tree.root, interactive_styled.node_id)
      .expect("box node for <interactive-example>");
  let after_box = find_box_node_by_styled_id(&layout.box_tree.root, after_styled.node_id)
    .expect("box node for <p id=after>");

  let interactive_rect =
    find_fragment_abs_bounds_by_box_id(&layout.fragment_tree.root, interactive_box.id, Point::ZERO)
      .expect("fragment for <interactive-example>");
  let after_rect =
    find_fragment_abs_bounds_by_box_id(&layout.fragment_tree.root, after_box.id, Point::ZERO)
      .expect("fragment for <p id=after>");

  let height = interactive_rect.height();
  assert!(
    (height - 100.0).abs() <= 0.5,
    "expected <interactive-example> to have specified height 100px; got height={height} bounds={interactive_rect:?}"
  );

  let delta = after_rect.y() - interactive_rect.y();
  assert!(
    delta >= 99.5,
    "expected <p id=after> to start at least 100px below <interactive-example>; got delta={delta} interactive={interactive_rect:?} after={after_rect:?}"
  );
}

