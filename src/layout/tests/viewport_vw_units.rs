use crate::dom::DomNodeType;
use crate::layout::utils::resolve_scrollbar_width;
use crate::style::cascade::StyledNode;
use crate::style::media::MediaType;
use crate::{
  BoxNode, ComputedStyle, FastRender, FastRenderConfig, FontConfig, FragmentContent, FragmentNode,
};

fn find_styled_node_id_for_dom_id(node: &StyledNode, id_value: &str) -> Option<usize> {
  if let DomNodeType::Element { attributes, .. } = &node.node.node_type {
    if attributes
      .iter()
      .any(|(k, v)| k.eq_ignore_ascii_case("id") && v == id_value)
    {
      return Some(node.node_id);
    }
  }

  for child in node.children.iter() {
    if let Some(found) = find_styled_node_id_for_dom_id(child, id_value) {
      return Some(found);
    }
  }

  None
}

fn find_box_id_for_styled_node_id(node: &BoxNode, styled_node_id: usize) -> Option<usize> {
  if node.generated_pseudo.is_none() && node.styled_node_id == Some(styled_node_id) {
    return Some(node.id);
  }
  for child in node.children.iter() {
    if let Some(found) = find_box_id_for_styled_node_id(child, styled_node_id) {
      return Some(found);
    }
  }
  if let Some(footnote_body) = node.footnote_body.as_deref() {
    if let Some(found) = find_box_id_for_styled_node_id(footnote_body, styled_node_id) {
      return Some(found);
    }
  }
  None
}

fn find_fragment_width_for_box_id(node: &FragmentNode, box_id: usize) -> Option<f32> {
  let matches_box = match &node.content {
    FragmentContent::Block { box_id: Some(id) }
    | FragmentContent::Inline {
      box_id: Some(id), ..
    }
    | FragmentContent::Text {
      box_id: Some(id), ..
    }
    | FragmentContent::Replaced {
      box_id: Some(id), ..
    } => *id == box_id,
    _ => false,
  };
  if matches_box {
    return Some(node.bounds.width());
  }

  for child in node.children.iter() {
    if let Some(found) = find_fragment_width_for_box_id(child, box_id) {
      return Some(found);
    }
  }

  None
}

fn element_width(html: &str, viewport: (u32, u32), element_id: &str) -> f32 {
  let config = FastRenderConfig::default().with_font_sources(FontConfig::bundled_only());
  let mut renderer = FastRender::with_config(config).expect("renderer");

  let dom = renderer.parse_html(html).expect("parse");
  let intermediates = renderer
    .layout_document_for_media_intermediates(&dom, viewport.0, viewport.1, MediaType::Screen)
    .expect("layout intermediates");

  let styled_node_id =
    find_styled_node_id_for_dom_id(&intermediates.styled_tree, element_id).expect("styled id");
  let box_id =
    find_box_id_for_styled_node_id(&intermediates.box_tree.root, styled_node_id).expect("box id");
  find_fragment_width_for_box_id(&intermediates.fragment_tree.root, box_id).expect("fragment width")
}

fn assert_close(actual: f32, expected: f32, label: &str) {
  let delta = (actual - expected).abs();
  assert!(
    delta < 0.1,
    "{label}: expected {expected:.2} got {actual:.2} (delta {delta:.2})"
  );
}

#[test]
fn viewport_units_use_outer_viewport_when_scrollbars_reserved() {
  // When the page opts into `scrollbar-gutter: stable`, the viewport reserves space for the
  // scrollbar gutter even when scrollbars are hidden/overlay. Percentage-based lengths on the root
  // element resolve against the reduced scrollport (`documentElement.clientWidth`), while viewport
  // units (`vw`) continue to resolve against the outer viewport (`window.innerWidth`).
  //
  // Time.com uses `w-screen` / `100vw` backgrounds that should still cover the scrollbar gutter.
  let html = r#"<!doctype html>
    <html>
      <head>
        <style>
          html, body { margin: 0; }
          html { scrollbar-gutter: stable; }
          body { overflow-y: auto; }
          #percent { width: 100%; height: 1px; }
          #vw { width: 100vw; height: 1px; }
          #spacer { height: 1000px; }
        </style>
      </head>
      <body>
        <div id="percent"></div>
        <div id="vw"></div>
        <div id="spacer"></div>
      </body>
    </html>"#;

  let viewport = (200, 100);
  let gutter = resolve_scrollbar_width(&ComputedStyle::default());

  let percent_width = element_width(html, viewport, "percent");
  assert_close(
    percent_width,
    viewport.0 as f32 - gutter,
    "percent width should use scrollport",
  );

  let vw_width = element_width(html, viewport, "vw");
  assert_close(
    vw_width,
    viewport.0 as f32,
    "vw width should use outer viewport",
  );
}
