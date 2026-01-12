use crate::dom::DomNodeType;
use crate::style::cascade::StyledNode;
use crate::style::media::MediaType;
use crate::{
  BoxNode, FastRender, FastRenderConfig, FontConfig, FragmentContent, FragmentNode, Rect,
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

fn find_box_node_for_box_id<'a>(node: &'a BoxNode, box_id: usize) -> Option<&'a BoxNode> {
  if node.id == box_id {
    return Some(node);
  }
  for child in node.children.iter() {
    if let Some(found) = find_box_node_for_box_id(child, box_id) {
      return Some(found);
    }
  }
  if let Some(footnote_body) = node.footnote_body.as_deref() {
    if let Some(found) = find_box_node_for_box_id(footnote_body, box_id) {
      return Some(found);
    }
  }
  None
}

fn find_fragment_bounds_for_box_id(node: &FragmentNode, box_id: usize) -> Option<Rect> {
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
    return Some(node.bounds);
  }

  for child in node.children.iter() {
    if let Some(found) = find_fragment_bounds_for_box_id(child, box_id) {
      return Some(found);
    }
  }

  None
}

#[test]
fn flex_auto_min_size_text_input_does_not_overflow_right_aligned_form() {
  // Regression test for berkeley.edu: a right-aligned flex item containing a text input should
  // be able to shrink below the input's default 20ch intrinsic width.
  let html = r#"<!doctype html>
    <html>
      <head>
        <style>
          html, body { margin: 0; }
          #form {
            display: flex;
            justify-content: flex-end;
            width: 210px;
          }
          #wrap { position: relative; }
          #field {
            width: 100%;
            border: 0;
            padding: 10px 100px 10px 60px;
          }
          #btn {
            position: absolute;
            right: 4px;
            top: 4px;
          }
        </style>
      </head>
      <body>
        <form id="form">
          <div id="wrap">
            <input id="field" type="search" />
            <button id="btn" type="submit">Go</button>
          </div>
        </form>
      </body>
    </html>"#;

  let config = FastRenderConfig::default().with_font_sources(FontConfig::bundled_only());
  let mut renderer = FastRender::with_config(config).expect("renderer");

  let dom = renderer.parse_html(html).expect("dom");
  let intermediates = renderer
    .layout_document_for_media_intermediates(&dom, 400, 200, MediaType::Screen)
    .expect("layout intermediates");

  let form_styled_id =
    find_styled_node_id_for_dom_id(&intermediates.styled_tree, "form").expect("form styled id");
  let wrap_styled_id =
    find_styled_node_id_for_dom_id(&intermediates.styled_tree, "wrap").expect("wrap styled id");

  let form_box_id = find_box_id_for_styled_node_id(&intermediates.box_tree.root, form_styled_id)
    .expect("form box id");
  let wrap_box_id = find_box_id_for_styled_node_id(&intermediates.box_tree.root, wrap_styled_id)
    .expect("wrap box id");

  let form_bounds = find_fragment_bounds_for_box_id(&intermediates.fragment_tree.root, form_box_id)
    .expect("form fragment");
  let wrap_bounds = find_fragment_bounds_for_box_id(&intermediates.fragment_tree.root, wrap_box_id)
    .expect("wrap fragment");

  // The wrap fragment bounds are in the coordinate system of the flex container (`#form`). Ensure
  // it does not end up at a negative x position due to an oversized min-content width.
  let eps = 0.5;
  assert!(
    wrap_bounds.x() >= -eps,
    "expected flex item to fit inside container: x={}",
    wrap_bounds.x()
  );
  assert!(
    wrap_bounds.x() + wrap_bounds.width() <= form_bounds.width() + eps,
    "expected flex item to fit inside container: right={} form_width={}",
    wrap_bounds.x() + wrap_bounds.width(),
    form_bounds.width()
  );
}

#[test]
fn flex_auto_min_size_appearance_none_text_inputs_do_not_push_button_outside() {
  // Regression test for yelp.com: `appearance:none` text inputs generate synthesized placeholder
  // text children, but their intrinsic min-content width must still behave like a form control so
  // they can shrink under flexbox's `min-width:auto` rules.
  let html = r#"<!doctype html>
    <html>
      <head>
        <style>
          html, body { margin: 0; }
          #form { display: flex; width: 240px; margin: 0; padding: 0; }
          #a, #b { appearance: none; flex: 1 1 auto; border: 0; padding: 8px; }
          #btn { flex: 0 0 auto; }
        </style>
      </head>
      <body>
        <form id="form">
          <input id="a" type="search" placeholder="AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA" />
          <input id="b" type="search" placeholder="BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB" />
          <button id="btn" type="submit">Search</button>
        </form>
      </body>
    </html>"#;

  let config = FastRenderConfig::default().with_font_sources(FontConfig::bundled_only());
  let mut renderer = FastRender::with_config(config).expect("renderer");

  let dom = renderer.parse_html(html).expect("dom");
  let intermediates = renderer
    .layout_document_for_media_intermediates(&dom, 400, 200, MediaType::Screen)
    .expect("layout intermediates");

  let form_styled_id =
    find_styled_node_id_for_dom_id(&intermediates.styled_tree, "form").expect("form styled id");
  let a_styled_id =
    find_styled_node_id_for_dom_id(&intermediates.styled_tree, "a").expect("a styled id");
  let b_styled_id =
    find_styled_node_id_for_dom_id(&intermediates.styled_tree, "b").expect("b styled id");
  let btn_styled_id =
    find_styled_node_id_for_dom_id(&intermediates.styled_tree, "btn").expect("btn styled id");

  let form_box_id = find_box_id_for_styled_node_id(&intermediates.box_tree.root, form_styled_id)
    .expect("form box id");
  let a_box_id =
    find_box_id_for_styled_node_id(&intermediates.box_tree.root, a_styled_id).expect("a box id");
  let b_box_id =
    find_box_id_for_styled_node_id(&intermediates.box_tree.root, b_styled_id).expect("b box id");
  let btn_box_id = find_box_id_for_styled_node_id(&intermediates.box_tree.root, btn_styled_id)
    .expect("btn box id");

  let a_box = find_box_node_for_box_id(&intermediates.box_tree.root, a_box_id).expect("a box");
  let b_box = find_box_node_for_box_id(&intermediates.box_tree.root, b_box_id).expect("b box");
  assert!(
    a_box.form_control.is_some() && b_box.form_control.is_some(),
    "expected appearance:none inputs to be represented as non-replaced form controls"
  );

  let form_bounds = find_fragment_bounds_for_box_id(&intermediates.fragment_tree.root, form_box_id)
    .expect("form fragment");
  let btn_bounds = find_fragment_bounds_for_box_id(&intermediates.fragment_tree.root, btn_box_id)
    .expect("btn fragment");

  let eps = 0.5;
  assert!(
    btn_bounds.x() >= -eps,
    "expected submit button to remain within container: x={}",
    btn_bounds.x()
  );
  assert!(
    btn_bounds.x() + btn_bounds.width() <= form_bounds.width() + eps,
    "expected submit button to remain within container: right={} form_width={}",
    btn_bounds.x() + btn_bounds.width(),
    form_bounds.width()
  );
}
