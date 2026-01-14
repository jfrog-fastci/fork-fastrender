use crate::tree::fragment_tree::{FragmentContent, FragmentNode};
use crate::{FastRender, FontConfig, Rgba};

fn find_text_fragment_abs_bounds(
  node: &FragmentNode,
  origin: (f32, f32),
  predicate: &impl Fn(&str, bool) -> bool,
) -> Option<(f32, f32, f32, f32)> {
  let abs_x = origin.0 + node.bounds.x();
  let abs_y = origin.1 + node.bounds.y();

  if let FragmentContent::Text {
    text, is_marker, ..
  } = &node.content
  {
    if predicate(text, *is_marker) {
      return Some((abs_x, abs_y, node.bounds.width(), node.bounds.height()));
    }
  }

  for child in node.children.iter() {
    if let Some(found) = find_text_fragment_abs_bounds(child, (abs_x, abs_y), predicate) {
      return Some(found);
    }
  }
  None
}

fn subtree_contains_background(node: &FragmentNode, color: Rgba) -> bool {
  if node
    .style
    .as_ref()
    .is_some_and(|style| style.background_color == color)
  {
    return true;
  }
  node
    .children
    .iter()
    .any(|child| subtree_contains_background(child, color))
}

fn find_line_containing_background<'a>(
  node: &'a FragmentNode,
  color: Rgba,
) -> Option<&'a FragmentNode> {
  if matches!(node.content, FragmentContent::Line { .. })
    && subtree_contains_background(node, color)
  {
    return Some(node);
  }
  for child in node.children.iter() {
    if let Some(found) = find_line_containing_background(child, color) {
      return Some(found);
    }
  }
  None
}

fn find_fragment_by_background<'a>(
  node: &'a FragmentNode,
  color: Rgba,
) -> Option<&'a FragmentNode> {
  if node
    .style
    .as_ref()
    .is_some_and(|style| style.background_color == color)
  {
    return Some(node);
  }
  for child in node.children.iter() {
    if let Some(found) = find_fragment_by_background(child, color) {
      return Some(found);
    }
  }
  None
}

#[test]
fn list_marker_outside_does_not_shift_list_item_content() {
  // arxiv.org is list-heavy; ensure default `list-style-position: outside` markers do not consume
  // inline width (i.e. the list item's content starts at the block start edge).
  let html = r#"
    <style>
      body { margin: 0; font-family: 'DejaVu Sans', sans-serif; font-size: 16px; line-height: 16px; }
      ul, li { margin: 0; padding: 0; }
    </style>
    <ul><li>Item</li></ul>
  "#;

  let mut renderer = FastRender::builder()
    .font_sources(FontConfig::bundled_only())
    .build()
    .expect("renderer");
  let dom = renderer.parse_html(html).expect("parse HTML");
  let fragments = renderer.layout_document(&dom, 200, 100).expect("layout");

  let (marker_x, _marker_y, marker_w, _marker_h) =
    find_text_fragment_abs_bounds(&fragments.root, (0.0, 0.0), &|text, is_marker| {
      is_marker && text.contains('•')
    })
    .expect("marker fragment");
  let (item_x, _item_y, _item_w, _item_h) =
    find_text_fragment_abs_bounds(&fragments.root, (0.0, 0.0), &|text, is_marker| {
      !is_marker && text.trim() == "Item"
    })
    .expect("item text fragment");

  assert!(
    item_x.abs() <= 0.5,
    "expected list item text to start at block start edge; got item_x={item_x}"
  );
  assert!(
    marker_x <= item_x - 0.5,
    "expected marker to be positioned to the left of list item content; marker_x={marker_x} marker_w={marker_w} item_x={item_x}",
  );
}

#[test]
fn inline_form_control_expands_line_box_height() {
  // arxiv.org contains inline form controls (inputs/selects/buttons) next to text. Even when the
  // line-height strut is small, an inline replaced element must expand the line box to avoid
  // overlap.
  let html = r#"
    <style>
      body { margin: 0; font-family: 'DejaVu Sans', sans-serif; font-size: 10px; line-height: 10px; }
      input {
        font-size: 10px;
        line-height: 10px;
        padding: 10px 0;
        border: 0;
        background: rgb(1, 2, 3);
      }
      p { margin: 0; }
    </style>
    <p>hi <input type="button" value="x"> there</p>
  "#;

  let target_color = Rgba::rgb(1, 2, 3);
  let mut renderer = FastRender::builder()
    .font_sources(FontConfig::bundled_only())
    .build()
    .expect("renderer");
  let dom = renderer.parse_html(html).expect("parse HTML");
  let fragments = renderer.layout_document(&dom, 200, 100).expect("layout");

  let input_fragment =
    find_fragment_by_background(&fragments.root, target_color).expect("input fragment");
  let line = find_line_containing_background(&fragments.root, target_color).expect("line fragment");

  assert!(
    line.bounds.height() + 0.5 >= input_fragment.bounds.height(),
    "expected line box to expand to fit inline input; line_h={} input_h={}",
    line.bounds.height(),
    input_fragment.bounds.height()
  );
}

#[test]
fn ua_select_and_button_default_metrics_match_chrome_compact_controls() {
  // arxiv.org relies on UA-styled <select>/<input type=button> controls for the homepage
  // "Subject search and browse" form. If our UA defaults are too large, the entire page's main
  // content gets pushed down and diffs explode. Keep the default geometry close to Chromium:
  // - select (non-multiple, size=1): ~19px tall
  // - button-like inputs: ~21px tall
  let select_bg = Rgba::rgb(1, 2, 3);
  let button_bg = Rgba::rgb(4, 5, 6);
  let html = format!(
    r#"
      <style>
        body {{ margin: 0; }}
        select {{ background: rgb({},{},{}); }}
        input {{ background: rgb({},{},{}); }}
      </style>
      <select>
        <option>Physics</option>
        <option>Electrical Engineering and Systems Science</option>
      </select>
      <input type="button" value="Search">
    "#,
    select_bg.r, select_bg.g, select_bg.b, button_bg.r, button_bg.g, button_bg.b
  );

  let mut renderer = FastRender::builder()
    .font_sources(FontConfig::bundled_only())
    .build()
    .expect("renderer");
  let dom = renderer.parse_html(&html).expect("parse HTML");
  let fragments = renderer.layout_document(&dom, 600, 200).expect("layout");

  let select_fragment = find_fragment_by_background(&fragments.root, select_bg).expect("select");
  let button_fragment = find_fragment_by_background(&fragments.root, button_bg).expect("button");

  assert!(
    (select_fragment.bounds.height() - 19.0).abs() <= 0.5,
    "expected UA select height ~= 19px, got {}",
    select_fragment.bounds.height()
  );
  assert!(
    (button_fragment.bounds.height() - 21.0).abs() <= 0.5,
    "expected UA input[type=button] height ~= 21px, got {}",
    button_fragment.bounds.height()
  );
}
