use super::generate_box_tree as generate_box_tree_result;
use super::generate_box_tree_with_anonymous_fixup as generate_box_tree_with_anonymous_fixup_result;
use super::*;
use crate::css::parser::extract_css;
use crate::debug::runtime::RuntimeToggles;
use crate::dom;
use crate::dom::HTML_NAMESPACE;
use crate::geometry::Size;
use crate::style;
use crate::style::cascade::apply_styles_with_media;
use crate::style::cascade::StartingStyleSet;
use crate::style::counter_styles::{CounterStyleRegistry, CounterStyleRule, CounterSystem};
use crate::style::counters::CounterSet;
use crate::style::media::MediaContext;
use crate::style::types::Appearance;
use crate::tree::box_tree::FormControl;
use crate::tree::box_tree::FormControlKind;
use crate::tree::box_tree::MarkerContent;
use crate::tree::box_tree::ReplacedType;
use crate::tree::box_tree::TextControlKind;

fn default_style() -> Arc<ComputedStyle> {
  Arc::new(ComputedStyle::default())
}

fn styled_element(tag: &str) -> StyledNode {
  StyledNode {
    node_id: 0,
    subtree_size: 1,
    node: DomNode {
      node_type: DomNodeType::Element {
        tag_name: tag.to_string(),
        namespace: HTML_NAMESPACE.to_string(),
        attributes: vec![],
      },
      children: vec![],
    },
    styles: Arc::new(ComputedStyle::default()),
    starting_styles: StartingStyleSet::default(),
    before_styles: None,
    after_styles: None,
    marker_styles: None,
    placeholder_styles: None,
    file_selector_button_styles: None,
    footnote_call_styles: None,
    footnote_marker_styles: None,
    first_line_styles: None,
    first_letter_styles: None,
    slider_thumb_styles: None,
    slider_track_styles: None,
    progress_bar_styles: None,
    progress_value_styles: None,
    meter_bar_styles: None,
    meter_optimum_value_styles: None,
    meter_suboptimum_value_styles: None,
    meter_even_less_good_value_styles: None,
    assigned_slot: None,
    slotted_node_ids: Vec::new(),
    children: vec![],
  }
}

fn generate_box_tree(styled: &StyledNode) -> BoxTree {
  generate_box_tree_result(styled).expect("box generation failed")
}

fn generate_box_tree_with_anonymous_fixup(styled: &StyledNode) -> BoxTree {
  generate_box_tree_with_anonymous_fixup_result(styled).expect("anonymous box generation failed")
}

fn serialized_inline_svg_content_from_html(html: &str, width: f32, height: f32) -> Option<SvgContent> {
  let dom = dom::parse_html(html).ok()?;
  let stylesheet = extract_css(&dom).ok()?;
  let media = MediaContext::screen(width, height);
  let styled = apply_styles_with_media(&dom, &stylesheet, &media);
  let box_tree = generate_box_tree_result(&styled).ok()?;

  fn find_svg(node: &BoxNode) -> Option<SvgContent> {
    if let BoxType::Replaced(repl) = &node.box_type {
      if let ReplacedType::Svg { content } = &repl.replaced_type {
        return Some(content.clone());
      }
    }
    for child in node.children.iter() {
      if let Some(content) = find_svg(child) {
        return Some(content);
      }
    }
    None
  }

  find_svg(&box_tree.root)
}

// Backwards-compatible helper for older inline-SVG tests.
fn serialized_inline_svg(html: &str, width: f32, height: f32) -> Option<SvgContent> {
  serialized_inline_svg_content_from_html(html, width, height)
}

fn count_object_replacements(node: &BoxNode) -> usize {
  let mut count = 0;
  if let BoxType::Replaced(repl) = &node.box_type {
    if matches!(repl.replaced_type, ReplacedType::Object { .. }) {
      count += 1;
    }
  }
  for child in node.children.iter() {
    count += count_object_replacements(child);
  }
  count
}

fn first_object_data(node: &BoxNode) -> Option<String> {
  if let BoxType::Replaced(repl) = &node.box_type {
    if let ReplacedType::Object { data } = &repl.replaced_type {
      return Some(data.clone());
    }
  }
  node.children.iter().find_map(first_object_data)
}

fn first_image_src(node: &BoxNode) -> Option<String> {
  if let BoxType::Replaced(repl) = &node.box_type {
    if let ReplacedType::Image { src, .. } = &repl.replaced_type {
      return Some(src.clone());
    }
  }
  node.children.iter().find_map(first_image_src)
}

fn first_video_src_and_poster(node: &BoxNode) -> Option<(String, Option<String>)> {
  if let BoxType::Replaced(repl) = &node.box_type {
    if let ReplacedType::Video { src, poster, .. } = &repl.replaced_type {
      return Some((src.clone(), poster.clone()));
    }
  }
  node.children.iter().find_map(first_video_src_and_poster)
}

fn collect_text(node: &BoxNode, out: &mut Vec<String>) {
  if let BoxType::Text(text) = &node.box_type {
    out.push(text.text.clone());
  }
  for child in node.children.iter() {
    collect_text(child, out);
  }
}

fn first_select_control_from_html(html: &str) -> FormControl {
  let dom = crate::dom::parse_html(html).expect("parse");
  let styled = crate::style::cascade::apply_styles(&dom, &crate::css::types::StyleSheet::new());
  let box_tree = generate_box_tree(&styled);

  fn find_select(node: &BoxNode) -> Option<FormControl> {
    if let BoxType::Replaced(repl) = &node.box_type {
      if let ReplacedType::FormControl(control) = &repl.replaced_type {
        if matches!(control.control, FormControlKind::Select(_)) {
          return Some(control.clone());
        }
      }
    }
    node.children.iter().find_map(find_select)
  }

  find_select(&box_tree.root).expect("expected select form control")
}

fn select_selected_value(select: &SelectControl) -> Option<&str> {
  let idx = select.selected.first().copied()?;
  match select.items.get(idx)? {
    SelectItem::Option { value, .. } => Some(value.as_str()),
    _ => None,
  }
}

fn collect_pseudo_text(
  node: &BoxNode,
  styled_node_id: usize,
  pseudo: GeneratedPseudoElement,
  out: &mut Vec<String>,
) {
  if node.styled_node_id == Some(styled_node_id)
    && node.generated_pseudo == Some(pseudo)
    && matches!(node.box_type, BoxType::Text(_))
  {
    if let BoxType::Text(text) = &node.box_type {
      out.push(text.text.clone());
    }
  }
  for child in node.children.iter() {
    collect_pseudo_text(child, styled_node_id, pseudo, out);
  }
}

fn pseudo_text(node: &BoxNode, styled_node_id: usize, pseudo: GeneratedPseudoElement) -> String {
  let mut parts = Vec::new();
  collect_pseudo_text(node, styled_node_id, pseudo, &mut parts);
  parts.join("")
}

fn marker_leading_decimal(marker: &str) -> i32 {
  let trimmed = marker.trim_start();
  let mut bytes = trimmed.as_bytes().iter().copied().peekable();
  let mut sign = 1i32;
  if matches!(bytes.peek(), Some(b'-')) {
    sign = -1;
    bytes.next();
  }

  let mut value: i32 = 0;
  let mut saw_digit = false;
  while let Some(b) = bytes.peek().copied() {
    if !b.is_ascii_digit() {
      break;
    }
    saw_digit = true;
    value = value.saturating_mul(10).saturating_add((b - b'0') as i32);
    bytes.next();
  }

  assert!(
    saw_digit,
    "expected marker to start with a decimal integer, got {:?}",
    marker
  );
  sign * value
}

#[test]
fn closed_details_without_summary_renders_default_legend_only() {
  let dom = crate::dom::parse_html("<!doctype html><details><div>Content</div></details>")
    .expect("parse");
  let styled = crate::style::cascade::apply_styles(&dom, &crate::css::types::StyleSheet::new());
  let box_tree = generate_box_tree(&styled);

  let mut text = Vec::new();
  collect_text(&box_tree.root, &mut text);
  let combined = text.join("");

  assert!(
    combined.contains("Details"),
    "expected closed <details> to render default legend text, got {combined:?}"
  );
  assert!(
    !combined.contains("Content"),
    "expected closed <details> to hide non-summary contents, got {combined:?}"
  );
}

#[test]
fn inline_svg_serialization_preserves_svg_template_children() {
  let html =
    "<!doctype html><html><body><svg><template><g id=hit></g></template></svg></body></html>";
  let content = serialized_inline_svg_content_from_html(html, 800.0, 600.0);
  let Some(content) = content else {
    panic!("expected inline SVG to produce SvgContent");
  };
  assert!(
    content.svg.contains("id=\"hit\""),
    "expected serialized SVG to preserve <template> descendants, got: {}",
    content.svg
  );
  assert!(
    content.svg.contains("<g"),
    "expected serialized SVG to contain <g element, got: {}",
    content.svg
  );
}

#[test]
fn svg_serialization_clears_invalid_var_in_style_attribute() {
  use crate::style::color::Rgba;
  use crate::style::types::{ColorOrNone, FillRule};

  fn styled_svg_element(tag: &str) -> StyledNode {
    let mut node = styled_element(tag);
    match &mut node.node.node_type {
      DomNodeType::Element { namespace, .. } => {
        *namespace = SVG_NAMESPACE.to_string();
      }
      _ => panic!("expected element node"),
    }
    node
  }

  let mut svg = styled_svg_element("svg");
  svg.node_id = 1;
  match &mut svg.node.node_type {
    DomNodeType::Element { attributes, .. } => {
      // Basic root attrs; ensure we don't short-circuit serialization.
      attributes.push(("viewBox".to_string(), "0 0 10 10".to_string()));
    }
    _ => unreachable!(),
  }

  let mut g = styled_svg_element("g");
  g.node_id = 2;
  match &mut g.node.node_type {
    DomNodeType::Element { attributes, .. } => {
      attributes.push(("fill".to_string(), "none".to_string()));
      attributes.push(("stroke".to_string(), "none".to_string()));
      attributes.push(("fill-rule".to_string(), "evenodd".to_string()));
      // Intentionally malformed var() call (unterminated) as seen in real-world fixtures.
      attributes.push((
        "style".to_string(),
        "fill: var(--q-colors-text-red;".to_string(),
      ));
    }
    _ => unreachable!(),
  }
  {
    let mut style = (*g.styles).clone();
    style.svg_fill = Some(ColorOrNone::None);
    style.svg_stroke = Some(ColorOrNone::None);
    style.svg_fill_rule = Some(FillRule::EvenOdd);
    style.color = Rgba::BLACK;
    g.styles = Arc::new(style);
  }

  let mut path = styled_svg_element("path");
  path.node_id = 3;
  match &mut path.node.node_type {
    DomNodeType::Element { attributes, .. } => {
      attributes.push(("d".to_string(), "M0 0 L10 0 L10 10 L0 10 Z".to_string()));
    }
    _ => unreachable!(),
  }

  g.children.push(path);
  svg.children.push(g);

  let content = serialize_svg_subtree(&svg, "", None);
  assert!(
    !content.svg.contains("var(--q-colors-text-red;"),
    "serialized SVG should not retain unterminated var() in style attribute: {}",
    content.svg
  );
  assert!(
    content.svg.contains("fill: none"),
    "expected serialized SVG to include computed `fill: none`: {}",
    content.svg
  );
}

#[test]
fn img_intrinsic_size_tracks_single_dimension_attributes() {
  fn set_attr(node: &mut StyledNode, name: &str, value: &str) {
    match &mut node.node.node_type {
      DomNodeType::Element { attributes, .. } => {
        attributes.push((name.to_string(), value.to_string()));
      }
      _ => panic!("expected element node"),
    }
  }

  let mut img_width = styled_element("img");
  img_width.node_id = 1;
  set_attr(&mut img_width, "src", "test.png");
  set_attr(&mut img_width, "width", "120");

  let mut img_height = styled_element("img");
  img_height.node_id = 2;
  set_attr(&mut img_height, "src", "test.png");
  set_attr(&mut img_height, "height", "80");

  let mut root = styled_element("div");
  root.children = vec![img_width, img_height];

  let tree = generate_box_tree(&root);
  assert_eq!(tree.root.children.len(), 2);

  let width_box = &tree.root.children[0];
  match &width_box.box_type {
    BoxType::Replaced(replaced) => {
      assert_eq!(replaced.intrinsic_size, Some(Size::new(120.0, 0.0)));
    }
    other => panic!("expected replaced box, got {other:?}"),
  }

  let height_box = &tree.root.children[1];
  match &height_box.box_type {
    BoxType::Replaced(replaced) => {
      assert_eq!(replaced.intrinsic_size, Some(Size::new(0.0, 80.0)));
    }
    other => panic!("expected replaced box, got {other:?}"),
  }
}

#[test]
fn img_intrinsic_size_parses_dimension_attributes_with_px_suffix() {
  fn set_attr(node: &mut StyledNode, name: &str, value: &str) {
    match &mut node.node.node_type {
      DomNodeType::Element { attributes, .. } => {
        attributes.push((name.to_string(), value.to_string()));
      }
      _ => panic!("expected element node"),
    }
  }

  let mut img_width = styled_element("img");
  img_width.node_id = 1;
  set_attr(&mut img_width, "src", "test.png");
  set_attr(&mut img_width, "width", "50px");

  let mut img_height = styled_element("img");
  img_height.node_id = 2;
  set_attr(&mut img_height, "src", "test.png");
  set_attr(&mut img_height, "height", " 75px");

  let mut root = styled_element("div");
  root.children = vec![img_width, img_height];

  let tree = generate_box_tree(&root);
  assert_eq!(tree.root.children.len(), 2);

  let width_box = &tree.root.children[0];
  match &width_box.box_type {
    BoxType::Replaced(replaced) => {
      assert_eq!(replaced.intrinsic_size, Some(Size::new(50.0, 0.0)));
    }
    other => panic!("expected replaced box, got {other:?}"),
  }

  let height_box = &tree.root.children[1];
  match &height_box.box_type {
    BoxType::Replaced(replaced) => {
      assert_eq!(replaced.intrinsic_size, Some(Size::new(0.0, 75.0)));
    }
    other => panic!("expected replaced box, got {other:?}"),
  }
}

#[test]
fn img_intrinsic_size_ignores_invalid_dimension_attributes() {
  fn set_attr(node: &mut StyledNode, name: &str, value: &str) {
    match &mut node.node.node_type {
      DomNodeType::Element { attributes, .. } => {
        attributes.push((name.to_string(), value.to_string()));
      }
      _ => panic!("expected element node"),
    }
  }

  let mut zero = styled_element("img");
  zero.node_id = 1;
  set_attr(&mut zero, "src", "test.png");
  set_attr(&mut zero, "width", "0");

  let mut nan = styled_element("img");
  nan.node_id = 2;
  set_attr(&mut nan, "src", "test.png");
  set_attr(&mut nan, "width", "NaN");

  let mut negative = styled_element("img");
  negative.node_id = 3;
  set_attr(&mut negative, "src", "test.png");
  set_attr(&mut negative, "width", "-10");
  set_attr(&mut negative, "height", "-20");

  let mut negative_width_only = styled_element("img");
  negative_width_only.node_id = 4;
  set_attr(&mut negative_width_only, "src", "test.png");
  set_attr(&mut negative_width_only, "width", "-10");
  set_attr(&mut negative_width_only, "height", "80");

  let mut root = styled_element("div");
  root.children = vec![zero, nan, negative, negative_width_only];

  let tree = generate_box_tree(&root);
  assert_eq!(tree.root.children.len(), 4);

  for idx in 0..3 {
    let node = &tree.root.children[idx];
    let BoxType::Replaced(replaced) = &node.box_type else {
      panic!("expected replaced box, got {:?}", node.box_type);
    };
    assert_eq!(
      replaced.intrinsic_size, None,
      "unexpected intrinsic size for img {idx}"
    );
    assert_eq!(
      replaced.aspect_ratio, None,
      "unexpected aspect ratio for img {idx}"
    );
  }

  let node = &tree.root.children[3];
  let BoxType::Replaced(replaced) = &node.box_type else {
    panic!("expected replaced box, got {:?}", node.box_type);
  };
  assert_eq!(replaced.intrinsic_size, Some(Size::new(0.0, 80.0)));
  assert_eq!(replaced.aspect_ratio, None);
}

#[test]
fn appearance_none_form_controls_generate_fallback_children() {
  fn set_attr(node: &mut StyledNode, name: &str, value: &str) {
    match &mut node.node.node_type {
      DomNodeType::Element { attributes, .. } => {
        attributes.push((name.to_string(), value.to_string()));
      }
      _ => panic!("expected element node"),
    }
  }

  let mut input_style = ComputedStyle::default();
  input_style.appearance = Appearance::None;

  let mut input = styled_element("input");
  input.node_id = 1;
  input.styles = Arc::new(input_style);
  set_attr(&mut input, "value", "x");
  set_attr(&mut input, "placeholder", "placeholder");

  let mut root = styled_element("div");
  root.children = vec![input];

  let tree = generate_box_tree(&root);
  assert_eq!(tree.root.children.len(), 1);

  let child = &tree.root.children[0];

  fn count_form_controls(node: &BoxNode) -> usize {
    let mut count = 0usize;
    if let BoxType::Replaced(repl) = &node.box_type {
      if matches!(repl.replaced_type, ReplacedType::FormControl(_)) {
        count += 1;
      }
    }
    for child in node.children.iter() {
      count += count_form_controls(child);
    }
    count
  }

  assert_eq!(
    count_form_controls(&tree.root),
    0,
    "appearance:none should disable native form control replacement"
  );

  fn has_text(node: &BoxNode, value: &str) -> bool {
    if node.text().is_some_and(|text| text == value) {
      return true;
    }
    node.children.iter().any(|child| has_text(child, value))
  }

  assert!(
    has_text(child, "x"),
    "expected appearance:none input to expose its value as a text node"
  );
}

#[test]
fn img_crossorigin_attribute_parses() {
  fn set_attr(node: &mut StyledNode, name: &str, value: &str) {
    match &mut node.node.node_type {
      DomNodeType::Element { attributes, .. } => {
        attributes.push((name.to_string(), value.to_string()));
      }
      _ => panic!("expected element node"),
    }
  }

  let mut img_none = styled_element("img");
  img_none.node_id = 1;
  set_attr(&mut img_none, "src", "/a.png");

  let mut img_empty = styled_element("img");
  img_empty.node_id = 2;
  set_attr(&mut img_empty, "src", "/a.png");
  set_attr(&mut img_empty, "crossorigin", "");

  let mut img_anonymous = styled_element("img");
  img_anonymous.node_id = 3;
  set_attr(&mut img_anonymous, "src", "/a.png");
  set_attr(&mut img_anonymous, "crossorigin", "anonymous");

  let mut img_invalid = styled_element("img");
  img_invalid.node_id = 4;
  set_attr(&mut img_invalid, "src", "/a.png");
  set_attr(&mut img_invalid, "crossorigin", "iNvAlId");

  let mut img_creds = styled_element("img");
  img_creds.node_id = 5;
  set_attr(&mut img_creds, "src", "/a.png");
  set_attr(&mut img_creds, "crossorigin", "UsE-CrEdEnTiAlS");

  let mut root = styled_element("div");
  root.children = vec![img_none, img_empty, img_anonymous, img_invalid, img_creds];

  let tree = generate_box_tree(&root);
  assert_eq!(tree.root.children.len(), 5);

  let expected = [
    CrossOriginAttribute::None,
    CrossOriginAttribute::Anonymous,
    CrossOriginAttribute::Anonymous,
    CrossOriginAttribute::Anonymous,
    CrossOriginAttribute::UseCredentials,
  ];

  for (idx, want) in expected.into_iter().enumerate() {
    let node = &tree.root.children[idx];
    match &node.box_type {
      BoxType::Replaced(replaced) => match &replaced.replaced_type {
        ReplacedType::Image { crossorigin, .. } => assert_eq!(*crossorigin, want),
        other => panic!("expected image replaced type, got {other:?}"),
      },
      other => panic!("expected replaced box, got {other:?}"),
    }
  }
}

#[test]
fn img_decoding_attribute_parses() {
  fn set_attr(node: &mut StyledNode, name: &str, value: &str) {
    match &mut node.node.node_type {
      DomNodeType::Element { attributes, .. } => {
        attributes.push((name.to_string(), value.to_string()));
      }
      _ => panic!("expected element node"),
    }
  }

  let mut img_default = styled_element("img");
  img_default.node_id = 1;
  set_attr(&mut img_default, "src", "/a.png");

  let mut img_empty = styled_element("img");
  img_empty.node_id = 2;
  set_attr(&mut img_empty, "src", "/a.png");
  set_attr(&mut img_empty, "decoding", "");

  let mut img_async = styled_element("img");
  img_async.node_id = 3;
  set_attr(&mut img_async, "src", "/a.png");
  set_attr(&mut img_async, "decoding", "async");

  let mut img_sync = styled_element("img");
  img_sync.node_id = 4;
  set_attr(&mut img_sync, "src", "/a.png");
  set_attr(&mut img_sync, "decoding", "SYNC");

  let mut img_auto = styled_element("img");
  img_auto.node_id = 5;
  set_attr(&mut img_auto, "src", "/a.png");
  set_attr(&mut img_auto, "decoding", "  auto ");

  let mut img_invalid = styled_element("img");
  img_invalid.node_id = 6;
  set_attr(&mut img_invalid, "src", "/a.png");
  set_attr(&mut img_invalid, "decoding", "wat");

  let mut root = styled_element("div");
  root.children = vec![
    img_default,
    img_empty,
    img_async,
    img_sync,
    img_auto,
    img_invalid,
  ];

  let tree = generate_box_tree(&root);
  assert_eq!(tree.root.children.len(), 6);

  let expected = [
    ImageDecodingAttribute::Auto,
    ImageDecodingAttribute::Auto,
    ImageDecodingAttribute::Async,
    ImageDecodingAttribute::Sync,
    ImageDecodingAttribute::Auto,
    ImageDecodingAttribute::Auto,
  ];

  for (idx, want) in expected.into_iter().enumerate() {
    let node = &tree.root.children[idx];
    match &node.box_type {
      BoxType::Replaced(replaced) => match &replaced.replaced_type {
        ReplacedType::Image { decoding, .. } => assert_eq!(*decoding, want),
        other => panic!("expected image replaced type, got {other:?}"),
      },
      other => panic!("expected replaced box, got {other:?}"),
    }
  }
}

#[test]
fn img_loading_attribute_parses() {
  fn set_attr(node: &mut StyledNode, name: &str, value: &str) {
    match &mut node.node.node_type {
      DomNodeType::Element { attributes, .. } => {
        attributes.push((name.to_string(), value.to_string()));
      }
      _ => panic!("expected element node"),
    }
  }

  let mut img_default = styled_element("img");
  img_default.node_id = 1;
  set_attr(&mut img_default, "src", "/a.png");

  let mut img_empty = styled_element("img");
  img_empty.node_id = 2;
  set_attr(&mut img_empty, "src", "/a.png");
  set_attr(&mut img_empty, "loading", "");

  let mut img_lazy = styled_element("img");
  img_lazy.node_id = 3;
  set_attr(&mut img_lazy, "src", "/a.png");
  set_attr(&mut img_lazy, "loading", "lazy");

  let mut img_eager = styled_element("img");
  img_eager.node_id = 4;
  set_attr(&mut img_eager, "src", "/a.png");
  set_attr(&mut img_eager, "loading", "EAGER");

  let mut img_auto = styled_element("img");
  img_auto.node_id = 5;
  set_attr(&mut img_auto, "src", "/a.png");
  set_attr(&mut img_auto, "loading", "  auto ");

  let mut img_invalid = styled_element("img");
  img_invalid.node_id = 6;
  set_attr(&mut img_invalid, "src", "/a.png");
  set_attr(&mut img_invalid, "loading", "wat");

  let mut root = styled_element("div");
  root.children = vec![
    img_default,
    img_empty,
    img_lazy,
    img_eager,
    img_auto,
    img_invalid,
  ];

  let tree = generate_box_tree(&root);
  assert_eq!(tree.root.children.len(), 6);

  let expected = [
    ImageLoadingAttribute::Auto,
    ImageLoadingAttribute::Auto,
    ImageLoadingAttribute::Lazy,
    ImageLoadingAttribute::Eager,
    ImageLoadingAttribute::Auto,
    ImageLoadingAttribute::Auto,
  ];

  for (idx, want) in expected.into_iter().enumerate() {
    let node = &tree.root.children[idx];
    match &node.box_type {
      BoxType::Replaced(replaced) => match &replaced.replaced_type {
        ReplacedType::Image { loading, .. } => assert_eq!(*loading, want),
        other => panic!("expected image replaced type, got {other:?}"),
      },
      other => panic!("expected replaced box, got {other:?}"),
    }
  }
}

#[test]
fn non_ascii_whitespace_img_crossorigin_does_not_trim_nbsp() {
  fn set_attr(node: &mut StyledNode, name: &str, value: &str) {
    match &mut node.node.node_type {
      DomNodeType::Element { attributes, .. } => {
        attributes.push((name.to_string(), value.to_string()));
      }
      _ => panic!("expected element node"),
    }
  }

  let nbsp = "\u{00A0}";
  let crossorigin = format!("{nbsp}use-credentials");

  let mut img = styled_element("img");
  img.node_id = 1;
  set_attr(&mut img, "src", "/a.png");
  set_attr(&mut img, "crossorigin", &crossorigin);

  let mut root = styled_element("div");
  root.children = vec![img];

  let tree = generate_box_tree(&root);
  let node = &tree.root.children[0];
  match &node.box_type {
    BoxType::Replaced(replaced) => match &replaced.replaced_type {
      ReplacedType::Image { crossorigin, .. } => {
        assert_eq!(*crossorigin, CrossOriginAttribute::Anonymous)
      }
      other => panic!("expected image replaced type, got {other:?}"),
    },
    other => panic!("expected replaced box, got {other:?}"),
  }
}

#[test]
fn box_generation_reuses_computed_style_arcs() {
  let root_style = Arc::new(ComputedStyle::default());
  let text_style = Arc::new(ComputedStyle::default());
  assert!(
    !Arc::ptr_eq(&root_style, &text_style),
    "test setup requires distinct style arcs"
  );

  let text_dom = DomNode {
    node_type: DomNodeType::Text {
      content: "hello".to_string(),
    },
    children: vec![],
  };
  let text_node = StyledNode {
    node_id: 1,
    subtree_size: 1,
    node: text_dom,
    styles: Arc::clone(&text_style),
    starting_styles: StartingStyleSet::default(),
    before_styles: None,
    after_styles: None,
    marker_styles: None,
    placeholder_styles: None,
    file_selector_button_styles: None,
    footnote_call_styles: None,
    footnote_marker_styles: None,
    first_line_styles: None,
    first_letter_styles: None,
    slider_thumb_styles: None,
    slider_track_styles: None,
    progress_bar_styles: None,
    progress_value_styles: None,
    meter_bar_styles: None,
    meter_optimum_value_styles: None,
    meter_suboptimum_value_styles: None,
    meter_even_less_good_value_styles: None,
    assigned_slot: None,
    slotted_node_ids: Vec::new(),
    children: vec![],
  };

  let root_dom = DomNode {
    node_type: DomNodeType::Element {
      tag_name: "div".to_string(),
      namespace: HTML_NAMESPACE.to_string(),
      attributes: vec![],
    },
    children: vec![],
  };
  let root_node = StyledNode {
    node_id: 0,
    subtree_size: 1,
    node: root_dom,
    styles: Arc::clone(&root_style),
    starting_styles: StartingStyleSet::default(),
    before_styles: None,
    after_styles: None,
    marker_styles: None,
    placeholder_styles: None,
    file_selector_button_styles: None,
    footnote_call_styles: None,
    footnote_marker_styles: None,
    first_line_styles: None,
    first_letter_styles: None,
    slider_thumb_styles: None,
    slider_track_styles: None,
    progress_bar_styles: None,
    progress_value_styles: None,
    meter_bar_styles: None,
    meter_optimum_value_styles: None,
    meter_suboptimum_value_styles: None,
    meter_even_less_good_value_styles: None,
    assigned_slot: None,
    slotted_node_ids: Vec::new(),
    children: vec![text_node],
  };

  fn find_text_box<'a>(node: &'a BoxNode) -> Option<&'a BoxNode> {
    if matches!(node.box_type, BoxType::Text(_)) {
      return Some(node);
    }
    node.children.iter().find_map(find_text_box)
  }

  let tree = generate_box_tree(&root_node);

  assert!(
    Arc::ptr_eq(&tree.root.style, &root_style),
    "expected box tree to reuse the computed style Arc for element boxes"
  );

  let text_box = find_text_box(&tree.root).expect("expected a text box");
  assert!(
    Arc::ptr_eq(&text_box.style, &text_style),
    "expected box tree to reuse the computed style Arc for text boxes"
  );
}

#[test]
fn generate_box_tree_includes_marker_from_marker_styles() {
  use style::color::Rgba;
  use style::display::Display;
  use style::types::ListStyleType;

  let mut li_style = ComputedStyle::default();
  li_style.display = Display::ListItem;
  li_style.list_style_type = ListStyleType::Decimal;

  let mut marker_style = ComputedStyle::default();
  marker_style.display = Display::Inline;
  marker_style.content = "✱".to_string();
  marker_style.color = Rgba::RED;

  let text_dom = dom::DomNode {
    node_type: dom::DomNodeType::Text {
      content: "Item".to_string(),
    },
    children: vec![],
  };
  let text_node = StyledNode {
    node_id: 0,
    subtree_size: 1,
    node: text_dom.clone(),
    styles: Arc::new(ComputedStyle::default()),
    starting_styles: StartingStyleSet::default(),
    before_styles: None,
    after_styles: None,
    marker_styles: None,
    placeholder_styles: None,
    file_selector_button_styles: None,
    footnote_call_styles: None,
    footnote_marker_styles: None,
    first_line_styles: None,
    first_letter_styles: None,
    slider_thumb_styles: None,
    slider_track_styles: None,
    progress_bar_styles: None,
    progress_value_styles: None,
    meter_bar_styles: None,
    meter_optimum_value_styles: None,
    meter_suboptimum_value_styles: None,
    meter_even_less_good_value_styles: None,
    assigned_slot: None,
    slotted_node_ids: Vec::new(),
    children: vec![],
  };

  let li_dom = dom::DomNode {
    node_type: dom::DomNodeType::Element {
      tag_name: "li".to_string(),
      namespace: HTML_NAMESPACE.to_string(),
      attributes: vec![],
    },
    children: vec![text_dom],
  };

  let li = StyledNode {
    node_id: 0,
    subtree_size: 1,
    node: li_dom,
    styles: Arc::new(li_style),
    starting_styles: StartingStyleSet::default(),
    before_styles: None,
    after_styles: None,
    marker_styles: Some(Arc::new(marker_style)),
    placeholder_styles: None,
    file_selector_button_styles: None,
    footnote_call_styles: None,
    footnote_marker_styles: None,
    first_line_styles: None,
    first_letter_styles: None,
    slider_thumb_styles: None,
    slider_track_styles: None,
    progress_bar_styles: None,
    progress_value_styles: None,
    meter_bar_styles: None,
    meter_optimum_value_styles: None,
    meter_suboptimum_value_styles: None,
    meter_even_less_good_value_styles: None,
    assigned_slot: None,
    slotted_node_ids: Vec::new(),
    children: vec![text_node],
  };

  let tree = generate_box_tree(&li);
  assert_eq!(tree.root.children.len(), 2);
  let marker = tree.root.children.first().expect("marker");
  assert!(matches!(marker.box_type, BoxType::Marker(_)));
  assert_eq!(marker.text(), Some("✱"));
  assert_eq!(marker.style.color, Rgba::RED);
}

#[test]
fn html_represents_nothing_elements_do_not_render_even_with_authored_display() {
  let html = "<html><head><style>script{display:block}script::before{content:'X'}</style></head><body><script>SHOULD_NOT_RENDER</script><p>OK</p></body></html>";
  let dom = dom::parse_html(html).expect("parse html");
  let stylesheet = extract_css(&dom).expect("extract css");
  let media = MediaContext::screen(800.0, 600.0);
  let styled = apply_styles_with_media(&dom, &stylesheet, &media);
  let box_tree = generate_box_tree(&styled);

  let mut texts = Vec::new();
  collect_text(&box_tree.root, &mut texts);
  let rendered = texts.join("");

  assert!(
    rendered.contains("OK"),
    "expected normal content to render, got: {rendered:?}"
  );
  assert!(
    !rendered.contains("SHOULD_NOT_RENDER"),
    "script contents must not render, got: {rendered:?}"
  );
  assert!(
    !rendered.contains('X'),
    "script pseudo-elements must not render, got: {rendered:?}"
  );
}

#[test]
fn audio_generates_replaced_box_with_fallback_size() {
  let html = "<html><body><audio controls src=\"sound.mp3\"></audio></body></html>";
  let dom = crate::dom::parse_html(html).expect("parse");
  let styled = crate::style::cascade::apply_styles(&dom, &crate::css::types::StyleSheet::new());
  let box_tree = generate_box_tree(&styled);

  fn find_audio(node: &BoxNode, out: &mut Vec<ReplacedBox>) {
    if let BoxType::Replaced(repl) = &node.box_type {
      if matches!(repl.replaced_type, ReplacedType::Audio { .. }) {
        out.push(repl.clone());
      }
    }
    for child in node.children.iter() {
      find_audio(child, out);
    }
  }

  let mut audios = Vec::new();
  find_audio(&box_tree.root, &mut audios);
  assert_eq!(audios.len(), 1, "expected one audio replaced box");
  let audio = &audios[0];
  assert_eq!(
    audio.intrinsic_size,
    Some(Size::new(300.0, 32.0)),
    "audio should get default UA size when none provided"
  );
  assert!(
    audio.no_intrinsic_ratio,
    "default UA audio size must not imply an intrinsic aspect ratio"
  );
  assert_eq!(
    audio.aspect_ratio, None,
    "audio intrinsic ratio should be absent unless explicitly provided"
  );
}

#[test]
fn object_without_data_renders_children_instead_of_replacement() {
  // When <object> lacks a data attribute, it should fall back to its children.
  let html = "<html><body><object><p id=\"fallback\">hi</p></object></body></html>";
  let dom = crate::dom::parse_html(html).expect("parse");
  let styled = crate::style::cascade::apply_styles(&dom, &crate::css::types::StyleSheet::new());
  let box_tree = generate_box_tree(&styled);

  assert_eq!(
    count_object_replacements(&box_tree.root),
    0,
    "object without data should render fallback content"
  );

  let mut texts = Vec::new();
  collect_text(&box_tree.root, &mut texts);
  assert!(
    texts.iter().any(|t| t.contains("hi")),
    "fallback text from object children should be present"
  );
}

#[test]
fn object_with_whitespace_data_renders_children() {
  let html = "<html><body><object data=\"   \"><p>fallback</p></object></body></html>";
  let dom = crate::dom::parse_html(html).expect("parse");
  let styled = crate::style::cascade::apply_styles(&dom, &crate::css::types::StyleSheet::new());
  let box_tree = generate_box_tree(&styled);

  assert_eq!(
    count_object_replacements(&box_tree.root),
    0,
    "object with whitespace-only data should render fallback content"
  );

  let mut texts = Vec::new();
  collect_text(&box_tree.root, &mut texts);
  assert!(
    texts.iter().any(|t| t.contains("fallback")),
    "fallback text should be present"
  );
}

#[test]
fn object_with_unsupported_type_renders_children_instead_of_replacement() {
  let html = "<html><body><object data=\"doc.pdf\" type=\"application/pdf\"><p>hi</p></object></body></html>";
  let dom = crate::dom::parse_html(html).expect("parse");
  let styled = crate::style::cascade::apply_styles(&dom, &crate::css::types::StyleSheet::new());
  let box_tree = generate_box_tree(&styled);

  assert_eq!(
    count_object_replacements(&box_tree.root),
    0,
    "object with unsupported type should render fallback content"
  );

  let mut texts = Vec::new();
  collect_text(&box_tree.root, &mut texts);
  assert!(
    texts.iter().any(|t| t.contains("hi")),
    "fallback text from object children should be present"
  );
}

#[test]
fn object_with_supported_image_type_still_replaced() {
  let html = "<html><body><object data=\"img.png\" type=\"image/png\"></object></body></html>";
  let dom = crate::dom::parse_html(html).expect("parse");
  let styled = crate::style::cascade::apply_styles(&dom, &crate::css::types::StyleSheet::new());
  let box_tree = generate_box_tree(&styled);

  assert_eq!(
    count_object_replacements(&box_tree.root),
    1,
    "object with supported image type should render as replaced content"
  );
}

#[test]
fn object_replaced_data_is_trimmed() {
  let html =
    "<html><body><object data=\"  img.png  \" type=\"image/png\"></object></body></html>";
  let dom = crate::dom::parse_html(html).expect("parse");
  let styled = crate::style::cascade::apply_styles(&dom, &crate::css::types::StyleSheet::new());
  let box_tree = generate_box_tree(&styled);

  assert_eq!(
    count_object_replacements(&box_tree.root),
    1,
    "object should be a replaced element"
  );
  assert_eq!(
    first_object_data(&box_tree.root).as_deref(),
    Some("img.png"),
    "replaced object data should be whitespace-trimmed"
  );
}

#[test]
fn object_with_unsupported_data_url_renders_children_when_type_missing() {
  let html =
    "<html><body><object data=\"data:application/pdf,hello\"><p>fallback</p></object></body></html>";
  let dom = crate::dom::parse_html(html).expect("parse");
  let styled = crate::style::cascade::apply_styles(&dom, &crate::css::types::StyleSheet::new());
  let box_tree = generate_box_tree(&styled);

  assert_eq!(
    count_object_replacements(&box_tree.root),
    0,
    "object data URLs with unsupported mediatypes should render fallback children when `type` is missing"
  );

  let mut texts = Vec::new();
  collect_text(&box_tree.root, &mut texts);
  assert!(
    texts.iter().any(|t| t.contains("fallback")),
    "fallback text should be present"
  );
}

#[test]
fn object_with_image_data_url_is_replaced_when_type_missing() {
  let html =
    "<html><body><object data=\"data:image/png,hello\"><p>fallback</p></object></body></html>";
  let dom = crate::dom::parse_html(html).expect("parse");
  let styled = crate::style::cascade::apply_styles(&dom, &crate::css::types::StyleSheet::new());
  let box_tree = generate_box_tree(&styled);

  assert_eq!(
    count_object_replacements(&box_tree.root),
    1,
    "object data URLs with supported image mediatypes should create replaced content even when `type` is missing"
  );

  let mut texts = Vec::new();
  collect_text(&box_tree.root, &mut texts);
  assert!(
    !texts.iter().any(|t| t.contains("fallback")),
    "fallback text should not be present when object is replaced"
  );
}

#[test]
fn img_replaced_src_is_trimmed() {
  let html = "<html><body><img src=\"  img.png  \"></body></html>";
  let dom = crate::dom::parse_html(html).expect("parse");
  let styled = crate::style::cascade::apply_styles(&dom, &crate::css::types::StyleSheet::new());
  let box_tree = generate_box_tree(&styled);

  assert_eq!(
    first_image_src(&box_tree.root).as_deref(),
    Some("img.png"),
    "img src should be whitespace-trimmed"
  );
}

#[test]
fn img_replaced_src_preserves_non_ascii_whitespace() {
  let nbsp = "\u{00A0}";
  let html = format!("<html><body><img src=\"img.png{nbsp}\"></body></html>");
  let dom = crate::dom::parse_html(&html).expect("parse");
  let styled = crate::style::cascade::apply_styles(&dom, &crate::css::types::StyleSheet::new());
  let box_tree = generate_box_tree(&styled);

  assert_eq!(
    first_image_src(&box_tree.root),
    Some(format!("img.png{nbsp}")),
    "img src should not trim non-ASCII whitespace like NBSP"
  );
}

#[test]
fn video_src_and_poster_are_trimmed() {
  let html =
    "<html><body><video src=\"  v.mp4  \" poster=\"  poster.png  \"></video></body></html>";
  let dom = crate::dom::parse_html(html).expect("parse");
  let styled = crate::style::cascade::apply_styles(&dom, &crate::css::types::StyleSheet::new());
  let box_tree = generate_box_tree(&styled);

  let (src, poster) =
    first_video_src_and_poster(&box_tree.root).expect("expected a replaced video");
  assert_eq!(src, "v.mp4", "video src should be whitespace-trimmed");
  assert_eq!(
    poster.as_deref(),
    Some("poster.png"),
    "video poster should be whitespace-trimmed"
  );
}

#[test]
fn video_src_and_poster_preserve_non_ascii_whitespace() {
  let nbsp = "\u{00A0}";
  let html = format!(
    "<html><body><video src=\"v.mp4{nbsp}\" poster=\"poster.png{nbsp}\"></video></body></html>"
  );
  let dom = crate::dom::parse_html(&html).expect("parse");
  let styled = crate::style::cascade::apply_styles(&dom, &crate::css::types::StyleSheet::new());
  let box_tree = generate_box_tree(&styled);

  let (src, poster) =
    first_video_src_and_poster(&box_tree.root).expect("expected a replaced video");
  assert_eq!(
    src,
    format!("v.mp4{nbsp}"),
    "video src should preserve NBSP"
  );
  assert_eq!(
    poster,
    Some(format!("poster.png{nbsp}")),
    "video poster should preserve NBSP"
  );
}

#[test]
fn object_with_supported_html_type_still_replaced() {
  let html =
    "<html><body><object data=\"doc.html\" type=\"text/html\"><p>hi</p></object></body></html>";
  let dom = crate::dom::parse_html(html).expect("parse");
  let styled = crate::style::cascade::apply_styles(&dom, &crate::css::types::StyleSheet::new());
  let box_tree = generate_box_tree(&styled);

  assert_eq!(
    count_object_replacements(&box_tree.root),
    1,
    "object with supported HTML type should render as replaced content"
  );

  let mut texts = Vec::new();
  collect_text(&box_tree.root, &mut texts);
  assert!(
    !texts.iter().any(|t| t.contains("hi")),
    "fallback text should not be present when object is replaced"
  );
}

#[test]
fn non_ascii_whitespace_object_type_does_not_trim_nbsp() {
  let nbsp = "\u{00A0}";
  let html = format!(
    "<html><body><object data=\"doc.html\" type=\"{nbsp}text/html\"><p>fallback</p></object></body></html>"
  );
  let dom = crate::dom::parse_html(&html).expect("parse");
  let styled = crate::style::cascade::apply_styles(&dom, &crate::css::types::StyleSheet::new());
  let box_tree = generate_box_tree(&styled);

  assert_eq!(
    count_object_replacements(&box_tree.root),
    0,
    "NBSP must not be treated as whitespace when checking object type hints"
  );

  let mut texts = Vec::new();
  collect_text(&box_tree.root, &mut texts);
  assert!(
    texts.iter().any(|t| t.contains("fallback")),
    "fallback text should be present when object is not replaced"
  );
}

#[test]
fn form_controls_generate_replaced_boxes_except_button() {
  let html = "<html><body>
    <input id=\"text\" value=\"hello\">
    <input type=\"checkbox\" checked>
    <input type=\"radio\" checked>
    <select><option selected>One</option><option>Two</option></select>
    <button>Go</button>
    <textarea>note</textarea>
  </body></html>";
  let dom = crate::dom::parse_html(html).expect("parse");
  let styled = crate::style::cascade::apply_styles(&dom, &crate::css::types::StyleSheet::new());
  let box_tree = generate_box_tree(&styled);

  fn collect_controls(node: &BoxNode, out: &mut Vec<ReplacedType>) {
    if let BoxType::Replaced(repl) = &node.box_type {
      if matches!(repl.replaced_type, ReplacedType::FormControl(_)) {
        out.push(repl.replaced_type.clone());
      }
    }
    for child in node.children.iter() {
      collect_controls(child, out);
    }
  }

  let mut controls = Vec::new();
  collect_controls(&box_tree.root, &mut controls);
  assert_eq!(
    controls.len(),
    5,
    "<button> should not generate a replaced form control"
  );

  let mut texts = Vec::new();
  collect_text(&box_tree.root, &mut texts);
  assert!(
    texts.iter().any(|t| t.contains("Go")),
    "expected <button> descendants to generate text boxes"
  );

  assert!(controls.iter().any(|c| matches!(
    c,
    ReplacedType::FormControl(FormControl {
      control: FormControlKind::Checkbox {
        is_radio: false,
        checked: true,
        ..
      },
      ..
    })
  )));

  assert!(controls.iter().any(|c| matches!(
    c,
    ReplacedType::FormControl(FormControl {
      control: FormControlKind::Select(select),
      ..
    }) if select.selected.first().copied().is_some_and(|idx| matches!(
      select.items.get(idx),
      Some(SelectItem::Option { label, .. }) if label == "One"
    ))
  )));
}

#[test]
fn input_image_generates_image_replaced_box() {
  let html = "<html><body><input type=\"image\" src=\"icon.png\" width=\"30\" height=\"40\" alt=\"Submit\"></body></html>";
  let dom = crate::dom::parse_html(html).expect("parse");
  let styled = crate::style::cascade::apply_styles(&dom, &crate::css::types::StyleSheet::new());
  let box_tree = generate_box_tree(&styled);

  fn count_form_controls(node: &BoxNode) -> usize {
    let mut count = 0;
    if let BoxType::Replaced(repl) = &node.box_type {
      if matches!(repl.replaced_type, ReplacedType::FormControl(_)) {
        count += 1;
      }
    }
    for child in node.children.iter() {
      count += count_form_controls(child);
    }
    count
  }

  fn find_image_with_src<'a>(node: &'a BoxNode, expected_src: &str) -> Option<&'a ReplacedBox> {
    if let BoxType::Replaced(repl) = &node.box_type {
      if let ReplacedType::Image { src, .. } = &repl.replaced_type {
        if src == expected_src {
          return Some(repl);
        }
      }
    }
    node
      .children
      .iter()
      .find_map(|child| find_image_with_src(child, expected_src))
  }

  assert_eq!(
    count_form_controls(&box_tree.root),
    0,
    "<input type=image> should not be generated as a native form control"
  );

  let replaced =
    find_image_with_src(&box_tree.root, "icon.png").expect("expected image replaced box");
  match &replaced.replaced_type {
    ReplacedType::Image { alt, .. } => {
      assert_eq!(alt.as_deref(), Some("Submit"));
    }
    other => panic!("expected image replaced type, got {other:?}"),
  }
  assert_eq!(
    replaced.intrinsic_size,
    Some(Size::new(30.0, 40.0)),
    "<input type=image> width/height attributes should populate intrinsic size hints"
  );
}

#[test]
fn range_form_controls_capture_slider_thumb_pseudo_styles() {
  use crate::css::parser::parse_stylesheet;
  use crate::style::values::Length;

  let html = "<html><body><input class=\"range\" type=\"range\" value=\"50\"></body></html>";
  let dom = crate::dom::parse_html(html).expect("parse");
  let stylesheet =
    parse_stylesheet(".range::-webkit-slider-thumb { width: 18px; }").expect("parse css");
  let styled = crate::style::cascade::apply_styles(&dom, &stylesheet);
  let box_tree = generate_box_tree(&styled);

  fn find_range_control<'a>(node: &'a BoxNode) -> Option<&'a FormControl> {
    if let BoxType::Replaced(repl) = &node.box_type {
      if let ReplacedType::FormControl(control) = &repl.replaced_type {
        if matches!(control.control, FormControlKind::Range { .. }) {
          return Some(control);
        }
      }
    }
    node.children.iter().find_map(find_range_control)
  }

  let control = find_range_control(&box_tree.root).expect("range control");
  let thumb_style = control
    .slider_thumb_style
    .as_ref()
    .expect("thumb pseudo styles should be captured");
  assert_eq!(thumb_style.width, Some(Length::px(18.0)));
}

#[test]
fn range_form_controls_capture_slider_track_pseudo_styles() {
  use crate::css::parser::parse_stylesheet;
  use crate::style::color::Rgba;
  use crate::style::values::Length;

  let html = "<html><body><input class=\"range\" type=\"range\" value=\"50\"></body></html>";
  let dom = crate::dom::parse_html(html).expect("parse");
  let stylesheet = parse_stylesheet(
    ".range::-webkit-slider-runnable-track { height: 6px; background-color: rgb(1, 2, 3); }",
  )
  .expect("parse css");
  let styled = crate::style::cascade::apply_styles(&dom, &stylesheet);
  let box_tree = generate_box_tree(&styled);

  fn find_range_control<'a>(node: &'a BoxNode) -> Option<&'a FormControl> {
    if let BoxType::Replaced(repl) = &node.box_type {
      if let ReplacedType::FormControl(control) = &repl.replaced_type {
        if matches!(control.control, FormControlKind::Range { .. }) {
          return Some(control);
        }
      }
    }
    node.children.iter().find_map(find_range_control)
  }

  let control = find_range_control(&box_tree.root).expect("range control");
  let track_style = control
    .slider_track_style
    .as_ref()
    .expect("track pseudo styles should be captured");
  assert_eq!(track_style.height, Some(Length::px(6.0)));
  assert_eq!(track_style.background_color, Rgba::new(1, 2, 3, 1.0));
}

#[test]
fn file_form_controls_capture_file_selector_button_pseudo_styles() {
  use crate::css::parser::parse_stylesheet;
  use crate::style::color::Rgba;

  let html = "<html><body><input class=\"file\" type=\"file\"></body></html>";
  let dom = crate::dom::parse_html(html).expect("parse");
  let stylesheet =
    parse_stylesheet(".file::-webkit-file-upload-button { background-color: rgb(11, 22, 33); }")
      .expect("parse css");
  let styled = crate::style::cascade::apply_styles(&dom, &stylesheet);
  let box_tree = generate_box_tree(&styled);

  fn find_file_control<'a>(node: &'a BoxNode) -> Option<&'a FormControl> {
    if let BoxType::Replaced(repl) = &node.box_type {
      if let ReplacedType::FormControl(control) = &repl.replaced_type {
        if matches!(control.control, FormControlKind::File { .. }) {
          return Some(control);
        }
      }
    }
    node.children.iter().find_map(find_file_control)
  }

  let control = find_file_control(&box_tree.root).expect("file input control");
  let button_style = control
    .file_selector_button_style
    .as_ref()
    .expect("file-selector-button pseudo styles should be captured");
  assert_eq!(button_style.background_color, Rgba::new(11, 22, 33, 1.0));
}

#[test]
fn text_inputs_capture_placeholder_pseudo_styles() {
  use crate::css::parser::parse_stylesheet;
  use crate::style::color::Rgba;

  let html = "<html><body><input placeholder=\"hello\"></body></html>";
  let dom = crate::dom::parse_html(html).expect("parse");
  let stylesheet =
    parse_stylesheet("input::placeholder { color: rgb(11, 22, 33); }").expect("parse css");
  let styled = crate::style::cascade::apply_styles(&dom, &stylesheet);
  let box_tree = generate_box_tree(&styled);

  fn find_text_control<'a>(node: &'a BoxNode) -> Option<&'a FormControl> {
    if let BoxType::Replaced(repl) = &node.box_type {
      if let ReplacedType::FormControl(control) = &repl.replaced_type {
        if matches!(control.control, FormControlKind::Text { .. }) {
          return Some(control);
        }
      }
    }
    node.children.iter().find_map(find_text_control)
  }

  let control = find_text_control(&box_tree.root).expect("text input control");
  let placeholder_style = control
    .placeholder_style
    .as_ref()
    .expect("placeholder pseudo styles should be captured");
  assert_eq!(placeholder_style.color, Rgba::new(11, 22, 33, 1.0));
}

#[test]
fn textareas_capture_placeholder_pseudo_styles() {
  use crate::css::parser::parse_stylesheet;
  use crate::style::color::Rgba;

  let html = "<html><body><textarea placeholder=\"hello\"></textarea></body></html>";
  let dom = crate::dom::parse_html(html).expect("parse");
  let stylesheet =
    parse_stylesheet("textarea::placeholder { color: rgb(44, 55, 66); }").expect("parse css");
  let styled = crate::style::cascade::apply_styles(&dom, &stylesheet);
  let box_tree = generate_box_tree(&styled);

  fn find_textarea_control<'a>(node: &'a BoxNode) -> Option<&'a FormControl> {
    if let BoxType::Replaced(repl) = &node.box_type {
      if let ReplacedType::FormControl(control) = &repl.replaced_type {
        if matches!(control.control, FormControlKind::TextArea { .. }) {
          return Some(control);
        }
      }
    }
    node.children.iter().find_map(find_textarea_control)
  }

  let control = find_textarea_control(&box_tree.root).expect("textarea control");
  let placeholder_style = control
    .placeholder_style
    .as_ref()
    .expect("placeholder pseudo styles should be captured");
  assert_eq!(placeholder_style.color, Rgba::new(44, 55, 66, 1.0));
}

#[test]
fn select_single_last_selected_wins() {
  let control = first_select_control_from_html(
    "<html><body><select><option selected>One</option><option selected>Two</option></select></body></html>",
  );
  let FormControlKind::Select(select) = &control.control else {
    panic!("expected select form control kind");
  };

  assert_eq!(select.selected.len(), 1);
  assert!(select.selected.first().copied().is_some_and(|idx| matches!(
    select.items.get(idx),
    Some(SelectItem::Option { label, .. }) if label == "Two"
  )));
  assert_eq!(
    select
      .items
      .iter()
      .filter(|item| matches!(item, SelectItem::Option { selected: true, .. }))
      .count(),
    1
  );
}

#[test]
fn select_single_defaults_to_first_non_disabled_option() {
  let control = first_select_control_from_html(
    "<html><body><select><option disabled>One</option><option>Two</option></select></body></html>",
  );
  let FormControlKind::Select(select) = &control.control else {
    panic!("expected select form control kind");
  };

  assert!(select.selected.first().copied().is_some_and(|idx| matches!(
    select.items.get(idx),
    Some(SelectItem::Option { label, .. }) if label == "Two"
  )));
}

#[test]
fn select_single_defaults_to_first_option_if_all_disabled() {
  let control = first_select_control_from_html(
    "<html><body><select><option disabled>One</option><option disabled>Two</option></select></body></html>",
  );
  let FormControlKind::Select(select) = &control.control else {
    panic!("expected select form control kind");
  };

  assert!(select.selected.first().copied().is_some_and(|idx| matches!(
    select.items.get(idx),
    Some(SelectItem::Option { label, .. }) if label == "One"
  )));
}

#[test]
fn required_select_with_disabled_selected_placeholder_stays_invalid() {
  let control = first_select_control_from_html(
    "<html><body><select required><option disabled selected value=\"\">Choose…</option><option value=\"a\">A</option></select></body></html>",
  );
  assert!(control.required);
  assert!(control.invalid);

  let FormControlKind::Select(select) = &control.control else {
    panic!("expected select form control kind");
  };
  assert!(select.selected.first().copied().is_some_and(|idx| matches!(
    select.items.get(idx),
    Some(SelectItem::Option { label, value, .. }) if label == "Choose…" && value.is_empty()
  )));
}

#[test]
fn required_select_with_first_option_in_optgroup_empty_value_is_valid() {
  let control = first_select_control_from_html(
    "<html><body><select required><optgroup label=\"g\"><option selected value=\"\">Empty</option></optgroup><option value=\"a\">A</option></select></body></html>",
  );
  assert!(control.required);
  assert!(!control.invalid);
}

#[test]
fn required_select_with_hidden_placeholder_option_is_valid() {
  let control = first_select_control_from_html(
    "<html><body><select required><option hidden value=\"\">Hidden</option><option value=\"a\">A</option></select></body></html>",
  );
  assert!(control.required);
  assert!(!control.invalid);
}

#[test]
fn required_multiple_select_without_selection_is_invalid() {
  let control = first_select_control_from_html(
    "<html><body><select multiple required><option>One</option><option>Two</option></select></body></html>",
  );
  assert!(control.required);
  assert!(control.invalid);
}

#[test]
fn required_multiple_select_with_only_hidden_selected_is_invalid() {
  let control = first_select_control_from_html(
    "<html><body><select multiple required><option hidden selected value=\"a\">A</option></select></body></html>",
  );
  assert!(control.required);
  assert!(control.invalid);
}

#[test]
fn required_multiple_select_with_selected_in_hidden_optgroup_is_invalid() {
  let control = first_select_control_from_html(
    "<html><body><select multiple required><optgroup hidden label=\"g\"><option selected value=\"a\">A</option></optgroup></select></body></html>",
  );
  assert!(control.required);
  assert!(control.invalid);
}

#[test]
fn required_multiple_select_with_only_disabled_selected_is_invalid() {
  let control = first_select_control_from_html(
    "<html><body><select multiple required><option selected disabled value=\"a\">A</option></select></body></html>",
  );
  assert!(control.required);
  assert!(control.invalid);
}

#[test]
fn required_multiple_select_with_selected_in_disabled_optgroup_is_invalid() {
  let control = first_select_control_from_html(
    "<html><body><select multiple required><optgroup disabled label=\"g\"><option selected value=\"a\">A</option></optgroup></select></body></html>",
  );
  assert!(control.required);
  assert!(control.invalid);
}

#[test]
fn required_multiple_select_with_selected_empty_value_is_valid() {
  let control = first_select_control_from_html(
    "<html><body><select multiple required><option selected value=\"\">A</option><option disabled selected value=\"b\">B</option></select></body></html>",
  );
  assert!(control.required);
  assert!(!control.invalid);
}

#[test]
fn required_select_with_selected_empty_value_not_placeholder_is_valid() {
  let control = first_select_control_from_html(
    "<html><body><select required><option value=\"a\">A</option><option selected value=\"\">Empty</option></select></body></html>",
  );
  assert!(control.required);
  assert!(!control.invalid);
}

#[test]
fn required_select_with_size_gt_1_selected_empty_value_is_valid() {
  let control = first_select_control_from_html(
    "<html><body><select required size=\"2\"><option selected value=\"\">Empty</option><option value=\"a\">A</option></select></body></html>",
  );
  assert!(control.required);
  assert!(!control.invalid);
}

#[test]
fn select_ignores_display_none_options() {
  let control = first_select_control_from_html(
    "<html><body><select size=\"4\"><option selected style=\"display:none\">One</option><option>Two</option></select></body></html>",
  );
  let FormControlKind::Select(select) = &control.control else {
    panic!("expected select form control kind");
  };

  assert_eq!(select.items.len(), 1);
  assert!(select.selected.first().copied().is_some_and(|idx| matches!(
    select.items.get(idx),
    Some(SelectItem::Option { label, .. }) if label == "Two"
  )));
}

#[test]
fn select_ignores_hidden_attribute_options() {
  let control = first_select_control_from_html(
    "<html><body><select size=\"4\"><option hidden>One</option><option>Two</option></select></body></html>",
  );
  let FormControlKind::Select(select) = &control.control else {
    panic!("expected select form control kind");
  };

  assert_eq!(select.items.len(), 1);
  assert!(select.selected.first().copied().is_some_and(|idx| matches!(
    select.items.get(idx),
    Some(SelectItem::Option { label, .. }) if label == "Two"
  )));
}

#[test]
fn select_ignores_hidden_optgroup_and_descendants() {
  let control = first_select_control_from_html(
    "<html><body><select size=\"4\"><optgroup hidden label=\"g\"><option>One</option></optgroup><option>Two</option></select></body></html>",
  );
  let FormControlKind::Select(select) = &control.control else {
    panic!("expected select form control kind");
  };

  assert_eq!(select.items.len(), 1);
  assert!(select.selected.first().copied().is_some_and(|idx| matches!(
    select.items.get(idx),
    Some(SelectItem::Option { label, .. }) if label == "Two"
  )));
}

#[test]
fn optgroup_disabled_propagates_and_does_not_clear_selected() {
  let control = first_select_control_from_html(
    "<html><body><select><optgroup label=\"g\" disabled><option selected>One</option></optgroup><option>Two</option></select></body></html>",
  );
  let FormControlKind::Select(select) = &control.control else {
    panic!("expected select form control kind");
  };

  assert!(select.items.iter().any(|item| matches!(
    item,
    SelectItem::OptGroupLabel { label, .. } if label == "g"
  )));
  assert!(select.selected.first().copied().is_some_and(|idx| matches!(
    select.items.get(idx),
    Some(SelectItem::Option { label, disabled: true, .. }) if label == "One"
  )));
  assert!(
    select.items.iter().any(|item| matches!(
      item,
      SelectItem::Option { label, in_optgroup: true, .. } if label == "One"
    )),
    "options under optgroup should be tagged as in_optgroup"
  );
  assert!(
    select.items.iter().any(|item| matches!(
      item,
      SelectItem::Option { label, in_optgroup: false, .. } if label == "Two"
    )),
    "options outside optgroup should not be tagged as in_optgroup"
  );
}

#[test]
fn select_multiple_keeps_all_selected_and_defaults_size() {
  let control = first_select_control_from_html(
    "<html><body><select multiple><option selected>One</option><option>Two</option><option selected disabled>Three</option></select></body></html>",
  );
  let FormControlKind::Select(select) = &control.control else {
    panic!("expected select form control kind");
  };

  assert!(select.multiple);
  assert_eq!(select.size, 4);
  assert_eq!(select.selected.len(), 2);
  assert!(select.selected.iter().all(|&idx| matches!(
    select.items.get(idx),
    Some(SelectItem::Option { selected: true, .. })
  )));
  assert!(select.items.iter().any(|item| matches!(
    item,
    SelectItem::Option { label, selected: true, disabled: true, .. } if label == "Three"
  )));
}

#[test]
fn select_size_attribute_defaults_and_parses() {
  let control = first_select_control_from_html(
    "<html><body><select multiple size=\"0\"><option selected>One</option></select></body></html>",
  );
  let FormControlKind::Select(select) = &control.control else {
    panic!("expected select form control kind");
  };
  assert_eq!(select.size, 4);

  let control = first_select_control_from_html(
    "<html><body><select size=\"0\"><option>One</option></select></body></html>",
  );
  let FormControlKind::Select(select) = &control.control else {
    panic!("expected select form control kind");
  };
  assert_eq!(select.size, 1);

  let control = first_select_control_from_html(
    "<html><body><select size=\"5\"><option>One</option></select></body></html>",
  );
  let FormControlKind::Select(select) = &control.control else {
    panic!("expected select form control kind");
  };
  assert_eq!(select.size, 5);
}

#[test]
fn select_option_label_prefers_label_attribute_over_text() {
  let control = first_select_control_from_html(
    "<html><body><select><option label=\"L\" value=\"v\">Text</option></select></body></html>",
  );
  let FormControlKind::Select(select) = &control.control else {
    panic!("expected select form control kind");
  };
  assert_eq!(select.items.len(), 1);
  assert!(matches!(
    select.items.first(),
    Some(SelectItem::Option { label, value, .. }) if label == "L" && value == "v"
  ));
}

#[test]
fn select_option_label_does_not_fallback_to_value() {
  let control = first_select_control_from_html(
    "<html><body><select><option value=\"v\"></option></select></body></html>",
  );
  let FormControlKind::Select(select) = &control.control else {
    panic!("expected select form control kind");
  };
  assert_eq!(select.items.len(), 1);
  assert!(matches!(
    select.items.first(),
    Some(SelectItem::Option { label, value, .. }) if label.is_empty() && value == "v"
  ));
}

#[test]
fn select_option_label_strips_and_collapses_ascii_whitespace() {
  let control = first_select_control_from_html(
    "<html><body><select><option>  Foo \n  Bar\tBaz  </option></select></body></html>",
  );
  let FormControlKind::Select(select) = &control.control else {
    panic!("expected select form control kind");
  };
  assert!(matches!(
    select.items.first(),
    Some(SelectItem::Option { label, .. }) if label == "Foo Bar Baz"
  ));
}

#[test]
fn select_option_label_empty_label_attribute_falls_back_to_text_even_with_value() {
  let control = first_select_control_from_html(
    "<html><body><select><option label=\"\" value=\"v\">Text</option></select></body></html>",
  );
  let FormControlKind::Select(select) = &control.control else {
    panic!("expected select form control kind");
  };
  assert!(matches!(
    select.items.first(),
    Some(SelectItem::Option { label, value, .. }) if label == "Text" && value == "v"
  ));
}

#[test]
fn select_option_value_defaults_to_stripped_text_content() {
  let control = first_select_control_from_html(
    "<html><body><select><option>  Foo \n Bar  </option></select></body></html>",
  );
  let FormControlKind::Select(select) = &control.control else {
    panic!("expected select form control kind");
  };
  assert!(matches!(
    select.items.first(),
    Some(SelectItem::Option { value, .. }) if value == "Foo Bar"
  ));
}

#[test]
fn select_option_label_empty_label_attribute_falls_back_to_text() {
  let control = first_select_control_from_html(
    "<html><body><select><option label=\"\">Text</option></select></body></html>",
  );
  let FormControlKind::Select(select) = &control.control else {
    panic!("expected select form control kind");
  };
  assert!(matches!(
    select.items.first(),
    Some(SelectItem::Option { label, value, .. }) if label == "Text" && value == "Text"
  ));
}

#[test]
fn select_option_label_collapses_internal_whitespace() {
  let control = first_select_control_from_html(
    "<html><body><select><option>Foo\n  Bar\tBaz</option></select></body></html>",
  );
  let FormControlKind::Select(select) = &control.control else {
    panic!("expected select form control kind");
  };
  assert!(matches!(
    select.items.first(),
    Some(SelectItem::Option { label, .. }) if label == "Foo Bar Baz"
  ));
}

#[test]
fn select_option_value_falls_back_to_option_text() {
  let control = first_select_control_from_html(
    "<html><body><select><option label=\"L\">  Foo \n</option></select></body></html>",
  );
  let FormControlKind::Select(select) = &control.control else {
    panic!("expected select form control kind");
  };
  assert!(matches!(
    select.items.first(),
    Some(SelectItem::Option { label, value, .. }) if label == "L" && value == "Foo"
  ));
}

#[test]
fn select_option_text_ignores_script_descendants() {
  fn styled_text(content: &str) -> StyledNode {
    StyledNode {
      node_id: 0,
      subtree_size: 1,
      node: DomNode {
        node_type: DomNodeType::Text {
          content: content.to_string(),
        },
        children: vec![],
      },
      styles: Arc::new(ComputedStyle::default()),
      starting_styles: StartingStyleSet::default(),
      before_styles: None,
      after_styles: None,
      marker_styles: None,
      placeholder_styles: None,
      file_selector_button_styles: None,
      footnote_call_styles: None,
      footnote_marker_styles: None,
      first_line_styles: None,
      first_letter_styles: None,
      slider_thumb_styles: None,
      slider_track_styles: None,
      progress_bar_styles: None,
      progress_value_styles: None,
      meter_bar_styles: None,
      meter_optimum_value_styles: None,
      meter_suboptimum_value_styles: None,
      meter_even_less_good_value_styles: None,
      assigned_slot: None,
      slotted_node_ids: Vec::new(),
      children: vec![],
    }
  }

  let mut select = styled_element("select");
  let mut option = styled_element("option");
  option.children = vec![
    styled_text("Foo "),
    {
      let mut script = styled_element("script");
      script.children = vec![styled_text("BAR")];
      script
    },
    styled_text(" Baz"),
  ];
  select.children = vec![option];

  let control = create_form_control_replaced(&select, &[], None)
    .expect("select should generate a form control")
    .control;
  let FormControlKind::Select(select) = &control else {
    panic!("expected select form control kind");
  };
  assert!(matches!(
    select.items.first(),
    Some(SelectItem::Option { label, value, .. }) if label == "Foo Baz" && value == "Foo Baz"
  ));
}

#[test]
fn select_option_label_attribute_is_verbatim() {
  let control = first_select_control_from_html(
    "<html><body><select><option label=\"  Foo  Bar  \">Text</option></select></body></html>",
  );
  let FormControlKind::Select(select) = &control.control else {
    panic!("expected select form control kind");
  };
  assert!(matches!(
    select.items.first(),
    Some(SelectItem::Option { label, value, .. }) if label == "  Foo  Bar  " && value == "Text"
  ));
}

#[test]
fn new_form_control_input_types_are_identified() {
  let html = "<html><body>
    <input type=\"password\" value=\"abc\">
    <input type=\"number\" value=\"5\">
    <input type=\"number\" value=\"abc\" placeholder=\"invalid number\">
    <input type=\"number\" value=\"abc\" required placeholder=\"required invalid number\">
    <input type=\"color\" value=\"#00ff00\">
    <input type=\"color\" value=\"not-a-color\">
    <input type=\"color\" value=\"not-a-color-disabled\" disabled>
    <input type=\"date\" required>
    <input type=\"date\" value=\"2020-13-01\" placeholder=\"invalid date\">
    <input type=\"date\" value=\"2020-13-01\" required placeholder=\"required invalid date\">
    <input type=\"datetime-local\">
    <input type=\"datetime-local\" value=\"2020-01-01T25:00\" placeholder=\"invalid datetime\">
    <input type=\"month\">
    <input type=\"month\" value=\"2020-13\" placeholder=\"invalid month\">
    <input type=\"week\">
    <input type=\"week\" value=\"2020-W99\" placeholder=\"invalid week\">
    <input type=\"time\">
    <input type=\"time\" value=\"25:00\" placeholder=\"invalid time\">
    <input type=\"number\" size=\"7\" placeholder=\"sized number\">
    <input type=\"checkbox\" indeterminate=\"true\">
    <input type=\"file\" value=\"C:\\\\fakepath\\\\hello.txt\">
    <input type=\"foo\" placeholder=\"mystery\">
    <input type=\"mystery\" value=\"abc\" placeholder=\"ph\" size=\"7\">
    <input size=\"5\" value=\"sized\">
    <textarea rows=\"4\" cols=\"10\">hi</textarea>
  </body></html>";
  let dom = crate::dom::parse_html(html).expect("parse");
  let styled = crate::style::cascade::apply_styles(&dom, &crate::css::types::StyleSheet::new());
  let focus_id = {
    let mut stack: Vec<&StyledNode> = vec![&styled];
    let mut found = None;
    while let Some(node) = stack.pop() {
      let is_number = node
        .node
        .tag_name()
        .is_some_and(|tag| tag.eq_ignore_ascii_case("input"))
        && node
          .node
          .get_attribute_ref("type")
          .is_some_and(|t| t.eq_ignore_ascii_case("number"))
        && node.node.get_attribute_ref("value") == Some("5");
      if is_number {
        found = Some(node.node_id);
        break;
      }
      stack.extend(node.children.iter());
    }
    found.expect("focused number input should exist in styled tree")
  };
  let mut interaction_state = InteractionState::default();
  interaction_state.focused = Some(focus_id);
  interaction_state.focus_visible = true;
  interaction_state.set_focus_chain(vec![focus_id]);
  let box_tree = generate_box_tree_with_options_and_interaction_state(
    &styled,
    &BoxGenerationOptions::default(),
    Some(&interaction_state),
  )
  .expect("box generation failed");

  fn collect_controls(node: &BoxNode, out: &mut Vec<FormControl>) {
    if let BoxType::Replaced(repl) = &node.box_type {
      if let ReplacedType::FormControl(control) = &repl.replaced_type {
        out.push(control.clone());
      }
    }
    for child in node.children.iter() {
      collect_controls(child, out);
    }
  }

  let mut controls = Vec::new();
  collect_controls(&box_tree.root, &mut controls);

  assert!(
    controls.iter().any(|c| matches!(
      &c.control,
      FormControlKind::Text {
        kind: TextControlKind::Password,
        ..
      }
    )),
    "password input should map to password text control"
  );
  assert!(
    controls.iter().any(|c| matches!(
      &c.control,
      FormControlKind::Text {
        kind: TextControlKind::Number,
        value,
        ..
      } if value == "5"
    ) && c.focus_visible),
    "number input should be recognized and keep focus-visible hint"
  );
  assert!(
    controls.iter().any(|c| {
      matches!(
        &c.control,
        FormControlKind::Text {
          kind: TextControlKind::Number,
          value,
          placeholder,
          ..
        } if value.is_empty() && placeholder.as_deref() == Some("invalid number")
      ) && !c.required
        && !c.invalid
    }),
    "invalid number input values should sanitize to empty and remain valid when not required"
  );
  assert!(
    controls.iter().any(|c| {
      matches!(
        &c.control,
        FormControlKind::Text {
          kind: TextControlKind::Number,
          value,
          placeholder,
          ..
        } if value.is_empty() && placeholder.as_deref() == Some("required invalid number")
      ) && c.required
        && c.invalid
    }),
    "required number input with invalid value should be marked invalid"
  );
  assert!(
    controls
      .iter()
      .any(|c| matches!(&c.control, FormControlKind::Color { .. })),
    "color input should generate a color control"
  );
  assert!(
    controls.iter().any(|c| {
      matches!(
        &c.control,
        FormControlKind::Color { raw, .. } if raw.as_deref() == Some("not-a-color")
      ) && !c.disabled
        && !c.invalid
    }),
    "color inputs sanitize invalid values and stay valid for painting"
  );
  assert!(
    controls.iter().any(|c| {
      matches!(
        &c.control,
        FormControlKind::Color { raw, .. } if raw.as_deref() == Some("not-a-color-disabled")
      ) && c.disabled
        && !c.invalid
    }),
    "disabled color inputs with invalid values should stay valid for painting"
  );
  assert!(
    controls.iter().any(|c| {
      matches!(
        &c.control,
        FormControlKind::Text {
          kind: TextControlKind::Date,
          value,
          placeholder,
          ..
        } if value.is_empty() && placeholder.as_deref() == Some("invalid date")
      ) && !c.required
        && !c.invalid
    }),
    "invalid date input values should sanitize to empty and remain valid when not required"
  );
  assert!(
    controls.iter().any(|c| {
      matches!(
        &c.control,
        FormControlKind::Text {
          kind: TextControlKind::Date,
          value,
          placeholder,
          ..
        } if value.is_empty() && placeholder.as_deref() == Some("required invalid date")
      ) && c.required
        && c.invalid
    }),
    "required date input with invalid value should be marked invalid"
  );
  assert!(
    controls.iter().any(|c| {
      matches!(
        &c.control,
        FormControlKind::Text {
          kind: TextControlKind::Date,
          value,
          placeholder,
          ..
        } if value.is_empty() && placeholder.as_deref() == Some("invalid datetime")
      ) && !c.required
        && !c.invalid
    }),
    "invalid datetime-local input values should sanitize to empty and remain valid when not required"
  );
  assert!(
    controls.iter().any(|c| {
      matches!(
        &c.control,
        FormControlKind::Text {
          kind: TextControlKind::Date,
          value,
          placeholder,
          ..
        } if value.is_empty() && placeholder.as_deref() == Some("invalid month")
      ) && !c.required
        && !c.invalid
    }),
    "invalid month input values should sanitize to empty and remain valid when not required"
  );
  assert!(
    controls.iter().any(|c| {
      matches!(
        &c.control,
        FormControlKind::Text {
          kind: TextControlKind::Date,
          value,
          placeholder,
          ..
        } if value.is_empty() && placeholder.as_deref() == Some("invalid week")
      ) && !c.required
        && !c.invalid
    }),
    "invalid week input values should sanitize to empty and remain valid when not required"
  );
  assert!(
    controls.iter().any(|c| {
      matches!(
        &c.control,
        FormControlKind::Text {
          kind: TextControlKind::Date,
          value,
          placeholder,
          ..
        } if value.is_empty() && placeholder.as_deref() == Some("invalid time")
      ) && !c.required
        && !c.invalid
    }),
    "invalid time input values should sanitize to empty and remain valid when not required"
  );
  assert!(
    controls.iter().any(|c| matches!(
      &c.control,
      FormControlKind::Text {
        kind: TextControlKind::Date,
        ..
      }
    ) && c.required
      && c.invalid),
    "required date input without value should be marked invalid"
  );
  assert!(
    controls.iter().any(|c| matches!(
      &c.control,
      FormControlKind::Text {
        kind: TextControlKind::Date,
        placeholder,
        ..
      } if placeholder.as_deref() == Some("yyyy-mm-dd hh:mm")
    )),
    "datetime-local inputs should synthesize a datetime placeholder"
  );
  assert!(
    controls.iter().any(|c| matches!(
      &c.control,
      FormControlKind::Text {
        kind: TextControlKind::Date,
        placeholder,
        ..
      } if placeholder.as_deref() == Some("yyyy-mm")
    )),
    "month inputs should synthesize a month placeholder"
  );
  assert!(
    controls.iter().any(|c| matches!(
      &c.control,
      FormControlKind::Text {
        kind: TextControlKind::Date,
        placeholder,
        ..
      } if placeholder.as_deref() == Some("yyyy-Www")
    )),
    "week inputs should synthesize a week placeholder"
  );
  assert!(
    controls.iter().any(|c| matches!(
      &c.control,
      FormControlKind::Text {
        kind: TextControlKind::Date,
        placeholder,
        ..
      } if placeholder.as_deref() == Some("hh:mm")
    )),
    "time inputs should synthesize a time placeholder"
  );
  assert!(
    controls.iter().any(|c| matches!(
      &c.control,
      FormControlKind::Checkbox {
        indeterminate: true,
        ..
      }
    )),
    "indeterminate checkbox should be captured"
  );
  assert!(
    controls
      .iter()
      .any(|c| matches!(&c.control, FormControlKind::File { value } if value.is_none())),
    "file inputs should be captured as file form controls (value is never pre-filled from markup)"
  );
  assert!(
    controls.iter().any(|c| matches!(
      &c.control,
      FormControlKind::Text {
        kind: TextControlKind::Plain,
        placeholder,
        value,
        ..
      } if placeholder.as_deref() == Some("mystery") && value.is_empty()
    )),
    "unknown types should fall back to plain text controls"
  );
  assert!(
    controls.iter().any(|c| matches!(
      &c.control,
      FormControlKind::Text {
        kind: TextControlKind::Plain,
        value,
        placeholder,
        size_attr: Some(7),
        ..
      } if value == "abc" && placeholder.as_deref() == Some("ph")
    )),
    "unknown input types should behave like type=text and preserve value/placeholder/size"
  );
  assert!(
    controls.iter().any(|c| matches!(
      &c.control,
      FormControlKind::Text {
        size_attr: Some(5),
        kind: TextControlKind::Plain,
        ..
      }
    )),
    "size attribute should be preserved on text-like inputs"
  );
  assert!(
    controls.iter().any(|c| matches!(
      &c.control,
      FormControlKind::Text {
        size_attr: Some(7),
        kind: TextControlKind::Number,
        placeholder,
        ..
      } if placeholder.as_deref() == Some("sized number")
    )),
    "number inputs should keep size hints and placeholder text"
  );
  assert!(
    controls.iter().any(|c| matches!(
      &c.control,
      FormControlKind::TextArea { rows, cols, .. } if rows == &Some(4) && cols == &Some(10)
    )),
    "rows/cols should be captured on textarea for intrinsic sizing"
  );
}

#[test]
fn form_state_overrides_text_input_value() {
  let html = "<html><body><input value=\"default\"></body></html>";
  let dom = crate::dom::parse_html(html).expect("parse");
  let styled = crate::style::cascade::apply_styles(&dom, &crate::css::types::StyleSheet::new());

  fn find_input_id(node: &StyledNode) -> Option<usize> {
    if node
      .node
      .tag_name()
      .is_some_and(|tag| tag.eq_ignore_ascii_case("input"))
    {
      return Some(node.node_id);
    }
    node.children.iter().find_map(find_input_id)
  }

  let input_id = find_input_id(&styled).expect("expected input element in styled tree");
  let mut interaction_state = InteractionState::default();
  interaction_state
    .form_state_mut()
    .values
    .insert(input_id, "override".to_string());

  let box_tree = generate_box_tree_with_options_and_interaction_state(
    &styled,
    &BoxGenerationOptions::default(),
    Some(&interaction_state),
  )
  .expect("box generation failed");

  fn find_text_control<'a>(node: &'a BoxNode) -> Option<&'a FormControl> {
    if let BoxType::Replaced(repl) = &node.box_type {
      if let ReplacedType::FormControl(control) = &repl.replaced_type {
        if matches!(control.control, FormControlKind::Text { .. }) {
          return Some(control);
        }
      }
    }
    node.children.iter().find_map(find_text_control)
  }

  let control = find_text_control(&box_tree.root).expect("expected text input form control");
  let FormControlKind::Text { value, .. } = &control.control else {
    panic!("expected text form control kind");
  };
  assert_eq!(value.as_str(), "override");
}

#[test]
fn form_state_overrides_checkbox_checkedness() {
  let html = "<html><body><input type=\"checkbox\" checked></body></html>";
  let dom = crate::dom::parse_html(html).expect("parse");
  let styled = crate::style::cascade::apply_styles(&dom, &crate::css::types::StyleSheet::new());

  fn find_checkbox_id(node: &StyledNode) -> Option<usize> {
    let is_checkbox = node
      .node
      .tag_name()
      .is_some_and(|tag| tag.eq_ignore_ascii_case("input"))
      && node
        .node
        .get_attribute_ref("type")
        .is_some_and(|t| t.eq_ignore_ascii_case("checkbox"));
    if is_checkbox {
      return Some(node.node_id);
    }
    node.children.iter().find_map(find_checkbox_id)
  }

  let checkbox_id = find_checkbox_id(&styled).expect("expected checkbox element in styled tree");
  let mut interaction_state = InteractionState::default();
  interaction_state
    .form_state_mut()
    .checked
    .insert(checkbox_id, false);

  let box_tree = generate_box_tree_with_options_and_interaction_state(
    &styled,
    &BoxGenerationOptions::default(),
    Some(&interaction_state),
  )
  .expect("box generation failed");

  fn find_checkbox_control<'a>(node: &'a BoxNode) -> Option<&'a FormControl> {
    if let BoxType::Replaced(repl) = &node.box_type {
      if let ReplacedType::FormControl(control) = &repl.replaced_type {
        if matches!(control.control, FormControlKind::Checkbox { .. }) {
          return Some(control);
        }
      }
    }
    node.children.iter().find_map(find_checkbox_control)
  }

  let control = find_checkbox_control(&box_tree.root).expect("expected checkbox form control");
  let FormControlKind::Checkbox { checked, .. } = &control.control else {
    panic!("expected checkbox form control kind");
  };
  assert!(!*checked, "expected form state override to clear checkedness");
}

#[test]
fn form_state_overrides_select_selected_options() {
  use rustc_hash::FxHashSet;

  let html =
    "<html><body><select><option value=\"one\">One</option><option value=\"two\">Two</option></select></body></html>";
  let dom = crate::dom::parse_html(html).expect("parse");
  let styled = crate::style::cascade::apply_styles(&dom, &crate::css::types::StyleSheet::new());

  fn find_select_id(node: &StyledNode) -> Option<usize> {
    if node
      .node
      .tag_name()
      .is_some_and(|tag| tag.eq_ignore_ascii_case("select"))
    {
      return Some(node.node_id);
    }
    node.children.iter().find_map(find_select_id)
  }

  fn collect_option_ids(node: &StyledNode, out: &mut Vec<usize>) {
    if node
      .node
      .tag_name()
      .is_some_and(|tag| tag.eq_ignore_ascii_case("option"))
    {
      out.push(node.node_id);
    }
    for child in &node.children {
      collect_option_ids(child, out);
    }
  }

  let select_id = find_select_id(&styled).expect("expected select element in styled tree");
  let mut option_ids = Vec::new();
  collect_option_ids(&styled, &mut option_ids);
  assert_eq!(option_ids.len(), 2, "expected two <option> elements");
  let option_one_id = option_ids[0];
  let option_two_id = option_ids[1];

  let mut selected = FxHashSet::default();
  selected.insert(option_two_id);
  let mut interaction_state = InteractionState::default();
  interaction_state
    .form_state_mut()
    .select_selected
    .insert(select_id, selected);

  let box_tree = generate_box_tree_with_options_and_interaction_state(
    &styled,
    &BoxGenerationOptions::default(),
    Some(&interaction_state),
  )
  .expect("box generation failed");

  fn find_select_control<'a>(node: &'a BoxNode) -> Option<&'a FormControl> {
    if let BoxType::Replaced(repl) = &node.box_type {
      if let ReplacedType::FormControl(control) = &repl.replaced_type {
        if matches!(control.control, FormControlKind::Select(_)) {
          return Some(control);
        }
      }
    }
    node.children.iter().find_map(find_select_control)
  }

  let control = find_select_control(&box_tree.root).expect("expected select form control");
  let FormControlKind::Select(select) = &control.control else {
    panic!("expected select form control kind");
  };
  assert_eq!(select_selected_value(select), Some("two"));

  assert!(
    select
      .items
      .iter()
      .any(|item| matches!(item, SelectItem::Option { node_id, selected: false, .. } if *node_id == option_one_id)),
    "expected first option to be deselected"
  );
  assert!(
    select
      .items
      .iter()
      .any(|item| matches!(item, SelectItem::Option { node_id, selected: true, .. } if *node_id == option_two_id)),
    "expected second option to be selected"
  );
}

#[test]
fn progress_and_meter_generate_form_control_replaced_boxes() {
  let html = "<html><body>
    <progress></progress>
    <progress value=\"15\" max=\"10\"></progress>
    <progress value=\"not-a-number\" max=\"10\"></progress>
    <meter value=\"200\" min=\"0\" max=\"100\" low=\"80\" high=\"20\" optimum=\"50\"></meter>
  </body></html>";
  let dom = crate::dom::parse_html(html).expect("parse");
  let styled = crate::style::cascade::apply_styles(&dom, &crate::css::types::StyleSheet::new());
  let box_tree = generate_box_tree(&styled);

  fn collect_controls(node: &BoxNode, out: &mut Vec<FormControl>) {
    if let BoxType::Replaced(repl) = &node.box_type {
      if let ReplacedType::FormControl(control) = &repl.replaced_type {
        out.push(control.clone());
      }
    }
    for child in node.children.iter() {
      collect_controls(child, out);
    }
  }

  let mut controls = Vec::new();
  collect_controls(&box_tree.root, &mut controls);

  assert!(
    controls.iter().any(|c| matches!(
      &c.control,
      FormControlKind::Progress { value, max } if *value < 0.0 && *max == 1.0
    )),
    "progress without value attribute should be represented as indeterminate"
  );
  assert!(
    controls.iter().any(|c| matches!(
      &c.control,
      FormControlKind::Progress { value, max } if *value == 10.0 && *max == 10.0
    )),
    "progress values should clamp into [0,max]"
  );
  assert!(
    controls.iter().any(|c| matches!(
      &c.control,
      FormControlKind::Progress { value, max } if *value < 0.0 && *max == 10.0
    )),
    "invalid progress value attribute should be treated as indeterminate"
  );
  assert!(
    controls.iter().any(|c| matches!(
      &c.control,
      FormControlKind::Meter { value, min, max, low, high, optimum }
        if *value == 100.0
          && *min == 0.0
          && *max == 100.0
          && *low == Some(20.0)
          && *high == Some(20.0)
          && *optimum == Some(50.0)
    )),
    "meter attributes should clamp and maintain low/high ordering"
  );
}

#[test]
fn button_elements_with_element_children_do_not_generate_replaced_form_controls() {
  let html = "<html><body><button><span>Icon</span></button></body></html>";
  let dom = crate::dom::parse_html(html).expect("parse");
  let styled = crate::style::cascade::apply_styles(&dom, &crate::css::types::StyleSheet::new());
  let box_tree = generate_box_tree(&styled);

  fn contains_form_control(node: &BoxNode) -> bool {
    if let BoxType::Replaced(repl) = &node.box_type {
      if matches!(repl.replaced_type, ReplacedType::FormControl(_)) {
        return true;
      }
    }
    node.children.iter().any(contains_form_control)
  }

  assert!(
    !contains_form_control(&box_tree.root),
    "expected <button> with element children to generate normal boxes, not a replaced form control"
  );

  let mut texts = Vec::new();
  collect_text(&box_tree.root, &mut texts);
  assert!(
    texts.iter().any(|t| t == "Icon"),
    "expected <button> contents to be preserved in the box tree (texts={texts:?})"
  );
}

#[test]
fn inline_buttons_are_atomic_inline_level_block_containers() {
  // Chrome/WebKit treat `<button style="display:inline">` as an atomic inline-level container for
  // its descendants (block descendants do not escape via CSS2 "block-in-inline splitting"). This
  // matters for real-world patterns where a button is styled like a link and contains flex/block
  // descendants.
  let html = "<html><body><p>Before <button style=\"display:inline\"><span style=\"display:flex\">FLEX</span></button> After</p></body></html>";
  let dom = crate::dom::parse_html(html).expect("parse");
  let styled = crate::style::cascade::apply_styles(&dom, &crate::css::types::StyleSheet::new());
  let box_tree = generate_box_tree_with_anonymous_fixup(&styled);

  fn find_button_id(node: &StyledNode) -> Option<usize> {
    if let DomNodeType::Element { tag_name, .. } = &node.node.node_type {
      if tag_name.eq_ignore_ascii_case("button") {
        return Some(node.node_id);
      }
    }
    node.children.iter().find_map(find_button_id)
  }

  fn find_box_by_styled_id<'a>(node: &'a BoxNode, id: usize) -> Option<&'a BoxNode> {
    if node.styled_node_id == Some(id) && node.generated_pseudo.is_none() {
      return Some(node);
    }
    node
      .children
      .iter()
      .find_map(|child| find_box_by_styled_id(child, id))
  }

  let button_id = find_button_id(&styled).expect("button node id");
  let button_box = find_box_by_styled_id(&box_tree.root, button_id)
    .expect("button box should remain present after anonymous fixup");
  assert_eq!(
    button_box.formatting_context(),
    Some(FormattingContextType::Block),
    "inline buttons should establish an internal block formatting context"
  );

  let mut texts = Vec::new();
  collect_text(button_box, &mut texts);
  assert!(
    texts.iter().any(|t| t == "FLEX"),
    "expected button subtree to contain its flex descendant text (texts={texts:?})"
  );
}

#[test]
fn input_type_image_is_treated_as_replaced_image() {
  let html = "<html><body><input type=\"image\" src=\"test.png\"></body></html>";
  let dom = crate::dom::parse_html(html).expect("parse");
  let styled = crate::style::cascade::apply_styles(&dom, &crate::css::types::StyleSheet::new());
  let box_tree = generate_box_tree(&styled);
  assert_eq!(
    first_image_src(&box_tree.root).as_deref(),
    Some("test.png"),
    "<input type=image> should be generated as an image replaced element"
  );
}

#[test]
fn form_control_values_use_html_sanitization_algorithms() {
  let html = "<html><body>
    <input type=\"range\" min=\"0\" max=\"10\" step=\"4\" value=\"10\">
    <textarea>\nhello\r\nworld</textarea>
    <textarea required> </textarea>
  </body></html>";
  let dom = crate::dom::parse_html(html).expect("parse");
  let styled = crate::style::cascade::apply_styles(&dom, &crate::css::types::StyleSheet::new());
  let box_tree = generate_box_tree(&styled);

  fn collect_controls(node: &BoxNode, out: &mut Vec<FormControl>) {
    if let BoxType::Replaced(repl) = &node.box_type {
      if let ReplacedType::FormControl(control) = &repl.replaced_type {
        out.push(control.clone());
      }
    }
    for child in node.children.iter() {
      collect_controls(child, out);
    }
  }

  let mut controls = Vec::new();
  collect_controls(&box_tree.root, &mut controls);

  assert!(
    controls.iter().any(|c| matches!(
      &c.control,
      FormControlKind::Range { min, max, value } if min == &0.0 && max == &10.0 && value == &8.0
    )),
    "range input should snap value to step and clamp within bounds"
  );

  assert!(
    controls.iter().any(|c| matches!(
      &c.control,
      FormControlKind::TextArea { value, .. } if value == "hello\nworld"
    )),
    "textarea values should normalize CRLF and strip the single leading newline"
  );

  assert!(
    controls.iter().any(|c| matches!(
      &c.control,
      FormControlKind::TextArea { value, .. } if value == " "
    ) && c.required
      && !c.invalid),
    "whitespace-only textarea should not fail required validation"
  );
}

#[test]
fn textarea_runtime_value_preserves_leading_newline_and_drives_placeholder_shown_matching() {
  use crate::css::parser::parse_stylesheet;
  use crate::dom::{DomNode, DomNodeType};
  use crate::style::cascade::{apply_styles_with_media, StyledNode};
  use crate::style::media::MediaContext;
  use crate::style::values::Length;

  fn find_by_tag<'a>(node: &'a StyledNode, tag: &str) -> Option<&'a StyledNode> {
    if let Some(name) = node.node.tag_name() {
      if name.eq_ignore_ascii_case(tag) {
        return Some(node);
      }
    }
    node.children.iter().find_map(|child| find_by_tag(child, tag))
  }

  fn find_first_element_mut<'a>(node: &'a mut DomNode, tag: &str) -> Option<&'a mut DomNode> {
    if node.tag_name().is_some_and(|t| t.eq_ignore_ascii_case(tag)) {
      return Some(node);
    }
    for child in node.children.iter_mut() {
      if let Some(found) = find_first_element_mut(child, tag) {
        return Some(found);
      }
    }
    None
  }

  fn set_attribute(node: &mut DomNode, name: &str, value: &str) {
    let attrs = match &mut node.node_type {
      DomNodeType::Element { attributes, .. } | DomNodeType::Slot { attributes, .. } => {
        attributes
      }
      _ => return,
    };

    if let Some((_, existing)) = attrs.iter_mut().find(|(k, _)| k.eq_ignore_ascii_case(name)) {
      existing.clear();
      existing.push_str(value);
      return;
    }
    attrs.push((name.to_string(), value.to_string()));
  }

  fn textarea_value<'a>(node: &'a BoxNode) -> Option<&'a str> {
    if let BoxType::Replaced(replaced) = &node.box_type {
      if let ReplacedType::FormControl(control) = &replaced.replaced_type {
        if let FormControlKind::TextArea { value, .. } = &control.control {
          return Some(value.as_str());
        }
      }
    }
    node.children.iter().find_map(textarea_value)
  }

  let css = r#"
    textarea { border-top-width: 1px; border-top-style: solid; }
    textarea:placeholder-shown { border-top-width: 2px; }
  "#;
  let sheet = parse_stylesheet(css).expect("parse stylesheet");

  // Empty textarea shows placeholder, so `:placeholder-shown` should match.
  let dom_empty = crate::dom::parse_html(r#"<textarea placeholder="p"></textarea>"#)
    .expect("parse html");
  let styled_empty = apply_styles_with_media(&dom_empty, &sheet, &MediaContext::screen(800.0, 600.0));
  let textarea_empty = find_by_tag(&styled_empty, "textarea").expect("textarea present");
  assert_eq!(textarea_empty.styles.border_top_width, Length::px(2.0));

  // Once the user has interacted with the control, we persist the value in `data-fastr-value`.
  // Leading newlines are part of the runtime value and must not be stripped.
  let mut dom =
    crate::dom::parse_html(r#"<textarea placeholder="p">x</textarea>"#).expect("parse html");
  let textarea = find_first_element_mut(&mut dom, "textarea").expect("textarea present");
  set_attribute(textarea, "data-fastr-value", "\nabc");

  let styled = apply_styles_with_media(&dom, &sheet, &MediaContext::screen(800.0, 600.0));
  let textarea_styled = find_by_tag(&styled, "textarea").expect("textarea present");
  // Non-empty runtime value => placeholder not shown.
  assert_eq!(textarea_styled.styles.border_top_width, Length::px(1.0));

  let box_tree = generate_box_tree(&styled);
  let value = textarea_value(&box_tree.root).expect("textarea control value");
  assert_eq!(value, "\nabc");
}

#[test]
fn appearance_none_disables_form_control_replacement_and_generates_placeholder_text() {
  let html =
    "<html><body><input id=\"plain\" placeholder=\"hello\" style=\"appearance: none; border: 0\"></body></html>";
  let dom = crate::dom::parse_html(html).expect("parse");
  let styled = crate::style::cascade::apply_styles(&dom, &crate::css::types::StyleSheet::new());
  let box_tree = generate_box_tree(&styled);

  fn count_replaced(node: &BoxNode) -> usize {
    let mut count = 0;
    if let BoxType::Replaced(repl) = &node.box_type {
      if matches!(repl.replaced_type, ReplacedType::FormControl(_)) {
        count += 1;
      }
    }
    for child in node.children.iter() {
      count += count_replaced(child);
    }
    count
  }

  assert_eq!(
    count_replaced(&box_tree.root),
    0,
    "appearance:none should disable native control replacement"
  );

  fn find_placeholder(node: &BoxNode) -> Option<&BoxNode> {
    if node.generated_pseudo == Some(GeneratedPseudoElement::Placeholder)
      && node.text() == Some("hello")
    {
      return Some(node);
    }
    node.children.iter().find_map(find_placeholder)
  }
  assert!(
    find_placeholder(&box_tree.root).is_some(),
    "expected placeholder text to be represented in the box tree when appearance:none"
  );
}

#[test]
fn webkit_appearance_none_disables_form_control_replacement() {
  let html =
    "<html><body><input id=\"plain\" placeholder=\"hello\" style=\"-webkit-appearance: none; border: 0\"></body></html>";
  let dom = crate::dom::parse_html(html).expect("parse");
  let styled = crate::style::cascade::apply_styles(&dom, &crate::css::types::StyleSheet::new());
  let box_tree = generate_box_tree(&styled);

  fn count_replaced(node: &BoxNode) -> usize {
    let mut count = 0;
    if let BoxType::Replaced(repl) = &node.box_type {
      if matches!(repl.replaced_type, ReplacedType::FormControl(_)) {
        count += 1;
      }
    }
    for child in node.children.iter() {
      count += count_replaced(child);
    }
    count
  }

  assert_eq!(
    count_replaced(&box_tree.root),
    0,
    "-webkit-appearance:none should disable native control replacement"
  );

  fn find_node_id_by_id_attr(
    node: &crate::style::cascade::StyledNode,
    id: &str,
  ) -> Option<usize> {
    if let DomNodeType::Element { attributes, .. } = &node.node.node_type {
      if attributes
        .iter()
        .any(|(name, value)| name == "id" && value == id)
      {
        return Some(node.node_id);
      }
    }
    node
      .children
      .iter()
      .find_map(|child| find_node_id_by_id_attr(child, id))
  }
  let input_node_id =
    find_node_id_by_id_attr(&styled, "plain").expect("expected <input id=plain> in styled tree");

  fn find_box_by_node_id<'a>(node: &'a BoxNode, node_id: usize) -> Option<&'a BoxNode> {
    if node.styled_node_id == Some(node_id) && node.generated_pseudo.is_none() {
      return Some(node);
    }
    node
      .children
      .iter()
      .find_map(|child| find_box_by_node_id(child, node_id))
  }
  let input_box = find_box_by_node_id(&box_tree.root, input_node_id)
    .expect("expected <input id=plain> to produce a box tree node");
  assert!(
    matches!(input_box.style.appearance, Appearance::None),
    "expected -webkit-appearance:none to compute to appearance:none"
  );
}

#[test]
fn moz_appearance_none_disables_form_control_replacement() {
  let html =
    "<html><body><input id=\"plain\" placeholder=\"hello\" style=\"-moz-appearance: none; border: 0\"></body></html>";
  let dom = crate::dom::parse_html(html).expect("parse");
  let styled = crate::style::cascade::apply_styles(&dom, &crate::css::types::StyleSheet::new());
  let box_tree = generate_box_tree(&styled);

  fn count_replaced(node: &BoxNode) -> usize {
    let mut count = 0;
    if let BoxType::Replaced(repl) = &node.box_type {
      if matches!(repl.replaced_type, ReplacedType::FormControl(_)) {
        count += 1;
      }
    }
    for child in node.children.iter() {
      count += count_replaced(child);
    }
    count
  }

  assert_eq!(
    count_replaced(&box_tree.root),
    0,
    "-moz-appearance:none should disable native control replacement"
  );

  fn find_node_id_by_id_attr(
    node: &crate::style::cascade::StyledNode,
    id: &str,
  ) -> Option<usize> {
    if let DomNodeType::Element { attributes, .. } = &node.node.node_type {
      if attributes
        .iter()
        .any(|(name, value)| name == "id" && value == id)
      {
        return Some(node.node_id);
      }
    }
    node
      .children
      .iter()
      .find_map(|child| find_node_id_by_id_attr(child, id))
  }
  let input_node_id =
    find_node_id_by_id_attr(&styled, "plain").expect("expected <input id=plain> in styled tree");

  fn find_box_by_node_id<'a>(node: &'a BoxNode, node_id: usize) -> Option<&'a BoxNode> {
    if node.styled_node_id == Some(node_id) && node.generated_pseudo.is_none() {
      return Some(node);
    }
    node
      .children
      .iter()
      .find_map(|child| find_box_by_node_id(child, node_id))
  }
  let input_box = find_box_by_node_id(&box_tree.root, input_node_id)
    .expect("expected <input id=plain> to produce a box tree node");
  assert!(
    matches!(input_box.style.appearance, Appearance::None),
    "expected -moz-appearance:none to compute to appearance:none"
  );
}

#[test]
fn replaced_media_defaults_to_300_by_150() {
  let style = default_style();

  for tag in ["canvas", "video", "iframe", "embed", "object"] {
    let mut styled = styled_element(tag);
    if tag == "object" {
      match &mut styled.node.node_type {
        DomNodeType::Element { attributes, .. } => {
          attributes.push(("data".to_string(), "data.bin".to_string()));
        }
        _ => panic!("expected element"),
      }
    }
    let box_node =
      create_replaced_box_from_styled(
        &styled,
        style.clone(),
        "",
        None,
        Vec::new(),
        &BoxGenerationOptions::default(),
        false,
      )
        .expect("expected replaced box");
    match &box_node.box_type {
      BoxType::Replaced(replaced) => {
        assert_eq!(
          replaced.intrinsic_size,
          Some(Size::new(300.0, 150.0)),
          "{tag} should default to 300x150"
        );
        match tag {
          "canvas" => {
            assert_eq!(
              replaced.aspect_ratio,
              Some(2.0),
              "canvas should default to 2:1 ratio"
            );
            assert!(
              !replaced.no_intrinsic_ratio,
              "canvas should have an intrinsic aspect ratio"
            );
          }
          _ => {
            assert_eq!(
              replaced.aspect_ratio, None,
              "{tag} default UA size must not imply an intrinsic aspect ratio"
            );
            assert!(
              replaced.no_intrinsic_ratio,
              "{tag} should not have an intrinsic aspect ratio by default"
            );
          }
        }
      }
      other => panic!("expected replaced box for {tag}, got {:?}", other),
    }
  }
}

#[test]
fn video_poster_falls_back_to_gnt_gl_ps_when_site_compat_enabled() {
  let mut styled = styled_element("video");
  match &mut styled.node.node_type {
    DomNodeType::Element { attributes, .. } => {
      attributes.push(("gnt-gl-ps".to_string(), "poster.png".to_string()));
    }
    _ => panic!("expected element"),
  }

  let box_node =
    create_replaced_box_from_styled(
      &styled,
      default_style(),
      "",
      None,
      Vec::new(),
      &BoxGenerationOptions::default(),
      true,
    )
      .expect("expected replaced box");
  match &box_node.box_type {
    BoxType::Replaced(replaced) => match &replaced.replaced_type {
      ReplacedType::Video { poster, .. } => {
        assert_eq!(poster.as_deref(), Some("poster.png"));
      }
      other => panic!("expected video replaced type, got {other:?}"),
    },
    other => panic!("expected replaced box, got {other:?}"),
  }
}

#[test]
fn video_poster_does_not_fall_back_to_gnt_gl_ps_when_site_compat_disabled() {
  let mut styled = styled_element("video");
  match &mut styled.node.node_type {
    DomNodeType::Element { attributes, .. } => {
      attributes.push(("gnt-gl-ps".to_string(), "poster.png".to_string()));
    }
    _ => panic!("expected element"),
  }

  let box_node =
    create_replaced_box_from_styled(
      &styled,
      default_style(),
      "",
      None,
      Vec::new(),
      &BoxGenerationOptions::default(),
      false,
    )
      .expect("expected replaced box");
  match &box_node.box_type {
    BoxType::Replaced(replaced) => match &replaced.replaced_type {
      ReplacedType::Video { poster, .. } => {
        assert_eq!(poster.as_deref(), None);
      }
      other => panic!("expected video replaced type, got {other:?}"),
    },
    other => panic!("expected replaced box, got {other:?}"),
  }
}

#[test]
fn video_src_falls_back_to_source_children() {
  let html =
    "<html><body><video><source src=\"a.mp4\"><source src=\"b.webm\"></video></body></html>";
  let dom = crate::dom::parse_html(html).expect("parse");
  let styled = crate::style::cascade::apply_styles(&dom, &crate::css::types::StyleSheet::new());
  let box_tree = generate_box_tree(&styled);

  fn find_video_src(node: &BoxNode) -> Option<String> {
    if let BoxType::Replaced(repl) = &node.box_type {
      if let ReplacedType::Video { src, .. } = &repl.replaced_type {
        return Some(src.clone());
      }
    }
    node.children.iter().find_map(find_video_src)
  }

  assert_eq!(find_video_src(&box_tree.root).as_deref(), Some("a.mp4"));
}

#[test]
fn video_src_placeholder_falls_back_to_source_children() {
  let html = "<html><body><video src=\"#\"><source src=\"a.mp4\"></video></body></html>";
  let dom = crate::dom::parse_html(html).expect("parse");
  let styled = crate::style::cascade::apply_styles(&dom, &crate::css::types::StyleSheet::new());
  let box_tree = generate_box_tree(&styled);

  fn find_video_src(node: &BoxNode) -> Option<String> {
    if let BoxType::Replaced(repl) = &node.box_type {
      if let ReplacedType::Video { src, .. } = &repl.replaced_type {
        return Some(src.clone());
      }
    }
    node.children.iter().find_map(find_video_src)
  }

  assert_eq!(find_video_src(&box_tree.root).as_deref(), Some("a.mp4"));
}

#[test]
fn video_src_fragment_only_falls_back_to_source_children() {
  let html = "<html><body><video src=\"#t=10\"><source src=\"a.mp4\"></video></body></html>";
  let dom = crate::dom::parse_html(html).expect("parse");
  let styled = crate::style::cascade::apply_styles(&dom, &crate::css::types::StyleSheet::new());
  let box_tree = generate_box_tree(&styled);

  fn find_video_src(node: &BoxNode) -> Option<String> {
    if let BoxType::Replaced(repl) = &node.box_type {
      if let ReplacedType::Video { src, .. } = &repl.replaced_type {
        return Some(src.clone());
      }
    }
    node.children.iter().find_map(find_video_src)
  }

  assert_eq!(find_video_src(&box_tree.root).as_deref(), Some("a.mp4"));
}

#[test]
fn audio_src_falls_back_to_source_children() {
  let html =
    "<html><body><audio controls><source src=\"a.mp3\"><source src=\"b.ogg\"></audio></body></html>";
  let dom = crate::dom::parse_html(html).expect("parse");
  let styled = crate::style::cascade::apply_styles(&dom, &crate::css::types::StyleSheet::new());
  let box_tree = generate_box_tree(&styled);

  fn find_audio_src(node: &BoxNode) -> Option<String> {
    if let BoxType::Replaced(repl) = &node.box_type {
      if let ReplacedType::Audio { src } = &repl.replaced_type {
        return Some(src.clone());
      }
    }
    node.children.iter().find_map(find_audio_src)
  }

  assert_eq!(find_audio_src(&box_tree.root).as_deref(), Some("a.mp3"));
}

#[test]
fn audio_src_placeholder_falls_back_to_source_children() {
  let html =
    "<html><body><audio controls src=\"about:blank\"><source src=\"a.mp3\"></audio></body></html>";
  let dom = crate::dom::parse_html(html).expect("parse");
  let styled = crate::style::cascade::apply_styles(&dom, &crate::css::types::StyleSheet::new());
  let box_tree = generate_box_tree(&styled);

  fn find_audio_src(node: &BoxNode) -> Option<String> {
    if let BoxType::Replaced(repl) = &node.box_type {
      if let ReplacedType::Audio { src } = &repl.replaced_type {
        return Some(src.clone());
      }
    }
    node.children.iter().find_map(find_audio_src)
  }

  assert_eq!(find_audio_src(&box_tree.root).as_deref(), Some("a.mp3"));
}

#[test]
fn audio_src_about_blank_fragment_falls_back_to_source_children() {
  let html =
    "<html><body><audio controls src=\"about:blank#foo\"><source src=\"a.mp3\"></audio></body></html>";
  let dom = crate::dom::parse_html(html).expect("parse");
  let styled = crate::style::cascade::apply_styles(&dom, &crate::css::types::StyleSheet::new());
  let box_tree = generate_box_tree(&styled);

  fn find_audio_src(node: &BoxNode) -> Option<String> {
    if let BoxType::Replaced(repl) = &node.box_type {
      if let ReplacedType::Audio { src } = &repl.replaced_type {
        return Some(src.clone());
      }
    }
    node.children.iter().find_map(find_audio_src)
  }

  assert_eq!(find_audio_src(&box_tree.root).as_deref(), Some("a.mp3"));
}

#[test]
fn video_src_prefers_source_type_prefix() {
  let html = "<html><body><video>
    <source src=\"wrong.mp4\" type=\"audio/mp3\">
    <source src=\"right.webm\" type=\"video/webm\">
  </video></body></html>";
  let dom = crate::dom::parse_html(html).expect("parse");
  let styled = crate::style::cascade::apply_styles(&dom, &crate::css::types::StyleSheet::new());
  let box_tree = generate_box_tree(&styled);

  fn find_video_src(node: &BoxNode) -> Option<String> {
    if let BoxType::Replaced(repl) = &node.box_type {
      if let ReplacedType::Video { src, .. } = &repl.replaced_type {
        return Some(src.clone());
      }
    }
    node.children.iter().find_map(find_video_src)
  }

  assert_eq!(
    find_video_src(&box_tree.root).as_deref(),
    Some("right.webm")
  );
}

#[test]
fn video_src_prefers_source_type_with_codecs() {
  let html = "<html><body><video>
    <source src=\"fallback.mp4\">
    <source src=\"preferred.webm\" type=\"video/webm; codecs=vp9\">
  </video></body></html>";
  let dom = crate::dom::parse_html(html).expect("parse");
  let styled = crate::style::cascade::apply_styles(&dom, &crate::css::types::StyleSheet::new());
  let box_tree = generate_box_tree(&styled);

  fn find_video_src(node: &BoxNode) -> Option<String> {
    if let BoxType::Replaced(repl) = &node.box_type {
      if let ReplacedType::Video { src, .. } = &repl.replaced_type {
        return Some(src.clone());
      }
    }
    node.children.iter().find_map(find_video_src)
  }

  assert_eq!(
    find_video_src(&box_tree.root).as_deref(),
    Some("preferred.webm")
  );
}

#[test]
fn video_src_prefers_playable_source_type_by_can_play_type() {
  let html = r#"<html><body><video>
    <source src="bad.webm" type="video/webm; codecs=bogus">
    <source src="good.mp4" type='video/mp4; codecs="avc1.42E01E, mp4a.40.2"'>
  </video></body></html>"#;
  let dom = crate::dom::parse_html(html).expect("parse");
  let styled = crate::style::cascade::apply_styles(&dom, &crate::css::types::StyleSheet::new());
  let box_tree = generate_box_tree(&styled);

  fn find_video_src(node: &BoxNode) -> Option<String> {
    if let BoxType::Replaced(repl) = &node.box_type {
      if let ReplacedType::Video { src, .. } = &repl.replaced_type {
        return Some(src.clone());
      }
    }
    node.children.iter().find_map(find_video_src)
  }

  assert_eq!(find_video_src(&box_tree.root).as_deref(), Some("good.mp4"));
}

#[test]
fn audio_src_prefers_source_type_prefix() {
  let html = "<html><body><audio controls>
    <source src=\"wrong.mp3\" type=\"video/mp4\">
    <source src=\"right.ogg\" type=\"audio/ogg\">
  </audio></body></html>";
  let dom = crate::dom::parse_html(html).expect("parse");
  let styled = crate::style::cascade::apply_styles(&dom, &crate::css::types::StyleSheet::new());
  let box_tree = generate_box_tree(&styled);

  fn find_audio_src(node: &BoxNode) -> Option<String> {
    if let BoxType::Replaced(repl) = &node.box_type {
      if let ReplacedType::Audio { src } = &repl.replaced_type {
        return Some(src.clone());
      }
    }
    node.children.iter().find_map(find_audio_src)
  }

  assert_eq!(find_audio_src(&box_tree.root).as_deref(), Some("right.ogg"));
}

#[test]
fn video_src_respects_source_media_queries_with_viewport_option() {
  use crate::css::types::StyleSheet;

  let html = "<html><body><video>
    <source src=\"wide.mp4\" media=\"(min-width: 600px)\">
    <source src=\"fallback.mp4\">
  </video></body></html>";
  let dom = crate::dom::parse_html(html).expect("parse");
  let stylesheet = StyleSheet::new();

  let styled_narrow =
    apply_styles_with_media(&dom, &stylesheet, &MediaContext::screen(500.0, 600.0));
  let options_narrow = BoxGenerationOptions::default().with_viewport(Size::new(500.0, 600.0));
  let tree_narrow =
    super::generate_box_tree_with_anonymous_fixup_with_options(&styled_narrow, &options_narrow)
      .expect("box tree");
  assert_eq!(
    first_video_src_and_poster(&tree_narrow.root).map(|(src, _)| src).as_deref(),
    Some("fallback.mp4")
  );

  let styled_wide = apply_styles_with_media(&dom, &stylesheet, &MediaContext::screen(800.0, 600.0));
  let options_wide = BoxGenerationOptions::default().with_viewport(Size::new(800.0, 600.0));
  let tree_wide =
    super::generate_box_tree_with_anonymous_fixup_with_options(&styled_wide, &options_wide)
      .expect("box tree");
  assert_eq!(
    first_video_src_and_poster(&tree_wide.root).map(|(src, _)| src).as_deref(),
    Some("wide.mp4")
  );
}

#[test]
fn audio_src_respects_source_media_queries_with_viewport_option() {
  use crate::css::types::StyleSheet;

  let html = "<html><body><audio controls>
    <source src=\"wide.mp3\" media=\"(min-width: 600px)\">
    <source src=\"fallback.mp3\">
  </audio></body></html>";
  let dom = crate::dom::parse_html(html).expect("parse");
  let stylesheet = StyleSheet::new();

  fn find_audio_src(node: &BoxNode) -> Option<String> {
    if let BoxType::Replaced(repl) = &node.box_type {
      if let ReplacedType::Audio { src } = &repl.replaced_type {
        return Some(src.clone());
      }
    }
    node.children.iter().find_map(find_audio_src)
  }

  let styled_narrow =
    apply_styles_with_media(&dom, &stylesheet, &MediaContext::screen(500.0, 600.0));
  let options_narrow = BoxGenerationOptions::default().with_viewport(Size::new(500.0, 600.0));
  let tree_narrow =
    super::generate_box_tree_with_anonymous_fixup_with_options(&styled_narrow, &options_narrow)
      .expect("box tree");
  assert_eq!(find_audio_src(&tree_narrow.root).as_deref(), Some("fallback.mp3"));

  let styled_wide = apply_styles_with_media(&dom, &stylesheet, &MediaContext::screen(800.0, 600.0));
  let options_wide = BoxGenerationOptions::default().with_viewport(Size::new(800.0, 600.0));
  let tree_wide =
    super::generate_box_tree_with_anonymous_fixup_with_options(&styled_wide, &options_wide)
      .expect("box tree");
  assert_eq!(find_audio_src(&tree_wide.root).as_deref(), Some("wide.mp3"));
}

#[test]
fn video_src_attribute_wins_over_source_children() {
  let html =
    "<html><body><video src=\"parent.mp4\"><source src=\"child.mp4\"></video></body></html>";
  let dom = crate::dom::parse_html(html).expect("parse");
  let styled = crate::style::cascade::apply_styles(&dom, &crate::css::types::StyleSheet::new());
  let box_tree = generate_box_tree(&styled);

  fn find_video_src(node: &BoxNode) -> Option<String> {
    if let BoxType::Replaced(repl) = &node.box_type {
      if let ReplacedType::Video { src, .. } = &repl.replaced_type {
        return Some(src.clone());
      }
    }
    node.children.iter().find_map(find_video_src)
  }

  assert_eq!(
    find_video_src(&box_tree.root).as_deref(),
    Some("parent.mp4")
  );
}

#[test]
fn video_src_with_media_fragment_wins_over_source_children() {
  let html =
    "<html><body><video src=\"parent.mp4#t=10\"><source src=\"child.mp4\"></video></body></html>";
  let dom = crate::dom::parse_html(html).expect("parse");
  let styled = crate::style::cascade::apply_styles(&dom, &crate::css::types::StyleSheet::new());
  let box_tree = generate_box_tree(&styled);

  fn find_video_src(node: &BoxNode) -> Option<String> {
    if let BoxType::Replaced(repl) = &node.box_type {
      if let ReplacedType::Video { src, .. } = &repl.replaced_type {
        return Some(src.clone());
      }
    }
    node.children.iter().find_map(find_video_src)
  }

  assert_eq!(
    find_video_src(&box_tree.root).as_deref(),
    Some("parent.mp4#t=10")
  );
}

#[test]
fn display_contents_splices_children_into_parent() {
  let mut root = styled_element("div");
  root.node_id = 1;
  Arc::make_mut(&mut root.styles).display = Display::Block;

  let mut contents = styled_element("section");
  contents.node_id = 2;
  Arc::make_mut(&mut contents.styles).display = Display::Contents;

  let mut child1 = styled_element("p");
  child1.node_id = 3;
  Arc::make_mut(&mut child1.styles).display = Display::Block;
  let mut child2 = styled_element("p");
  child2.node_id = 4;
  Arc::make_mut(&mut child2.styles).display = Display::Block;

  contents.children = vec![child1, child2];
  root.children = vec![contents];

  let tree = generate_box_tree(&root);
  assert_eq!(
    tree.root.children.len(),
    2,
    "contents element should not create a box"
  );
  let styled_ids: Vec<_> = tree
    .root
    .children
    .iter()
    .map(|c| c.styled_node_id)
    .collect();
  assert_eq!(styled_ids, vec![Some(3), Some(4)]);
}

#[test]
fn whitespace_text_nodes_are_preserved() {
  let mut root = styled_element("div");
  Arc::make_mut(&mut root.styles).display = Display::Block;
  let mut child = styled_element("span");
  Arc::make_mut(&mut child.styles).display = Display::Inline;
  child.node = dom::DomNode {
    node_type: dom::DomNodeType::Text {
      content: "   ".to_string(),
    },
    children: vec![],
  };
  root.children = vec![child];

  let tree = generate_box_tree(&root);
  assert_eq!(tree.root.children.len(), 1);
  assert!(
    tree.root.children[0].is_text(),
    "whitespace text should produce a text box"
  );
}

#[test]
fn non_ascii_whitespace_effective_content_value_does_not_trim_nbsp() {
  let nbsp = "\u{00A0}";
  let mut style = ComputedStyle::default();
  style.content_value = ContentValue::Normal;
  style.content = format!("{nbsp}none");
  assert_eq!(
    effective_content_value(&style),
    ContentValue::Items(vec![ContentItem::String(format!("{nbsp}none"))]),
    "NBSP must not be treated as CSS whitespace when interpreting legacy content strings"
  );
}

// =============================================================================
// Utility Method Tests
// =============================================================================

#[cfg(any(test, feature = "box_generation_demo"))]
#[test]
fn test_count_boxes_utility() {
  let style = default_style();

  // Single box
  let single = BoxNode::new_block(style.clone(), FormattingContextType::Block, vec![]);
  assert_eq!(BoxGenerator::count_boxes(&single), 1);

  // Parent with 3 children
  let children = vec![
    BoxNode::new_block(style.clone(), FormattingContextType::Block, vec![]),
    BoxNode::new_inline(style.clone(), vec![]),
    BoxNode::new_text(style.clone(), "text".to_string()),
  ];
  let parent = BoxNode::new_block(style, FormattingContextType::Block, children);
  assert_eq!(BoxGenerator::count_boxes(&parent), 4);
}

#[cfg(any(test, feature = "box_generation_demo"))]
#[test]
fn test_find_boxes_by_predicate() {
  let style = default_style();

  let text1 = BoxNode::new_text(style.clone(), "Hello".to_string());
  let text2 = BoxNode::new_text(style.clone(), "World".to_string());
  let inline = BoxNode::new_inline(style.clone(), vec![text1]);
  let block = BoxNode::new_block(
    style.clone(),
    FormattingContextType::Block,
    vec![inline, text2],
  );

  // Find all text boxes
  let text_boxes = BoxGenerator::find_boxes_by_predicate(&block, |b| b.is_text());
  assert_eq!(text_boxes.len(), 2);

  // Find inline boxes (inline + 2 text boxes since text is inline-level)
  let inline_boxes = BoxGenerator::find_boxes_by_predicate(&block, |b| b.is_inline_level());
  assert_eq!(inline_boxes.len(), 3); // 1 inline + 2 text boxes
}

#[test]
fn test_find_block_boxes() {
  let style = default_style();

  let text = BoxNode::new_text(style.clone(), "text".to_string());
  let inline = BoxNode::new_inline(style.clone(), vec![]);
  let block1 = BoxNode::new_block(style.clone(), FormattingContextType::Block, vec![]);
  let block2 = BoxNode::new_block(style.clone(), FormattingContextType::Flex, vec![]);
  let root = BoxNode::new_block(
    style,
    FormattingContextType::Block,
    vec![text, inline, block1, block2],
  );

  let blocks = BoxGenerator::find_block_boxes(&root);
  assert_eq!(blocks.len(), 3); // root + block1 + block2
}

#[test]
fn test_find_inline_boxes() {
  let style = default_style();

  let text = BoxNode::new_text(style.clone(), "text".to_string());
  let inline1 = BoxNode::new_inline(style.clone(), vec![text]);
  let inline2 = BoxNode::new_inline(style.clone(), vec![]);
  let root = BoxNode::new_block(style, FormattingContextType::Block, vec![inline1, inline2]);

  // inline1 + inline2 + text (text is also inline-level)
  let inlines = BoxGenerator::find_inline_boxes(&root);
  assert_eq!(inlines.len(), 3);
}

#[test]
fn test_find_text_boxes() {
  let style = default_style();

  let text1 = BoxNode::new_text(style.clone(), "Hello".to_string());
  let text2 = BoxNode::new_text(style.clone(), "World".to_string());
  let inline = BoxNode::new_inline(style.clone(), vec![text1]);
  let root = BoxNode::new_block(style, FormattingContextType::Block, vec![inline, text2]);

  let texts = BoxGenerator::find_text_boxes(&root);
  assert_eq!(texts.len(), 2);
}

#[test]
fn test_find_replaced_boxes_utility() {
  let style = default_style();

  let img = BoxNode::new_replaced(
    style.clone(),
    ReplacedType::Image {
      src: "test.png".to_string(),
      alt: None,
      loading: Default::default(),
      decoding: ImageDecodingAttribute::Auto,
      crossorigin: CrossOriginAttribute::None,
      referrer_policy: None,
      sizes: None,
      srcset: Vec::new(),
      picture_sources: Vec::new(),
    },
    Some(Size::new(100.0, 100.0)),
    Some(1.0),
  );
  let video = BoxNode::new_replaced(
    style.clone(),
    ReplacedType::Video {
      src: "test.mp4".to_string(),
      poster: None,
      controls: false,
    },
    None,
    None,
  );
  let text = BoxNode::new_text(style.clone(), "text".to_string());
  let root = BoxNode::new_block(style, FormattingContextType::Block, vec![img, video, text]);

  let replaced = BoxGenerator::find_replaced_boxes(&root);
  assert_eq!(replaced.len(), 2);
}
#[test]
fn fallback_marker_resets_box_model_but_inherits_color() {
  let mut li_style = ComputedStyle::default();
  li_style.display = Display::ListItem;
  li_style.color = crate::style::color::Rgba::RED;
  li_style.padding_left = crate::style::values::Length::px(20.0);
  li_style.margin_left = Some(crate::style::values::Length::px(10.0));

  let styled = StyledNode {
    node_id: 0,
    subtree_size: 1,
    node: dom::DomNode {
      node_type: dom::DomNodeType::Element {
        tag_name: "li".to_string(),
        namespace: HTML_NAMESPACE.to_string(),
        attributes: vec![],
      },
      children: vec![],
    },
    styles: Arc::new(li_style),
    starting_styles: StartingStyleSet::default(),
    before_styles: None,
    after_styles: None,
    marker_styles: None,
    placeholder_styles: None,
    file_selector_button_styles: None,
    footnote_call_styles: None,
    footnote_marker_styles: None,
    first_line_styles: None,
    first_letter_styles: None,
    slider_thumb_styles: None,
    slider_track_styles: None,
    progress_bar_styles: None,
    progress_value_styles: None,
    meter_bar_styles: None,
    meter_optimum_value_styles: None,
    meter_suboptimum_value_styles: None,
    meter_even_less_good_value_styles: None,
    assigned_slot: None,
    slotted_node_ids: Vec::new(),
    children: vec![],
  };

  let tree = generate_box_tree(&styled);
  let marker = match tree.root.children.first().expect("marker").box_type {
    BoxType::Marker(_) => tree.root.children.first().unwrap(),
    _ => panic!("expected marker as first child"),
  };
  assert_eq!(marker.style.color, crate::style::color::Rgba::RED);
  assert!(marker.style.padding_left.is_zero());
  assert!(marker.style.margin_left.unwrap().is_zero());
  assert_eq!(
    marker.style.background_color,
    crate::style::color::Rgba::TRANSPARENT
  );
}

#[test]
fn marker_styles_keep_text_decorations_and_shadows() {
  use crate::css::types::TextShadow;
  use crate::style::color::Rgba;
  use crate::style::counters::CounterManager;
  use crate::style::types::ListStyleType;
  use crate::style::types::TextDecorationLine;
  use crate::style::types::TextDecorationStyle;
  use crate::style::types::TextDecorationThickness;
  use crate::style::values::Length;

  let mut li_style = ComputedStyle::default();
  li_style.display = Display::ListItem;
  li_style.list_style_type = ListStyleType::Decimal;

  let mut marker_styles = ComputedStyle::default();
  marker_styles.display = Display::Inline;
  marker_styles.list_style_type = ListStyleType::Decimal;
  marker_styles.text_decoration.lines = TextDecorationLine::UNDERLINE;
  marker_styles.text_decoration.style = TextDecorationStyle::Wavy;
  marker_styles.text_decoration.color = Some(Rgba::BLUE);
  marker_styles.text_decoration.thickness = TextDecorationThickness::Length(Length::px(2.0));
  marker_styles.text_shadow = vec![TextShadow {
    offset_x: Length::px(1.0),
    offset_y: Length::px(2.0),
    blur_radius: Length::px(3.0),
    color: Some(Rgba::GREEN),
  }]
  .into();
  marker_styles.padding_left = Length::px(8.0);
  marker_styles.margin_left = Some(Length::px(4.0));
  marker_styles.background_color = Rgba::rgb(255, 0, 255);

  let styled = StyledNode {
    node_id: 0,
    subtree_size: 1,
    node: dom::DomNode {
      node_type: dom::DomNodeType::Element {
        tag_name: "li".to_string(),
        namespace: HTML_NAMESPACE.to_string(),
        attributes: vec![],
      },
      children: vec![],
    },
    styles: Arc::new(li_style),
    starting_styles: StartingStyleSet::default(),
    before_styles: None,
    after_styles: None,
    marker_styles: Some(Arc::new(marker_styles)),
    placeholder_styles: None,
    file_selector_button_styles: None,
    footnote_call_styles: None,
    footnote_marker_styles: None,
    first_line_styles: None,
    first_letter_styles: None,
    slider_thumb_styles: None,
    slider_track_styles: None,
    progress_bar_styles: None,
    progress_value_styles: None,
    meter_bar_styles: None,
    meter_optimum_value_styles: None,
    meter_suboptimum_value_styles: None,
    meter_even_less_good_value_styles: None,
    assigned_slot: None,
    slotted_node_ids: Vec::new(),
    children: vec![],
  };

  let mut quote_depth = 0usize;
  let marker_box = create_marker_box(&styled, &mut CounterManager::default(), &mut quote_depth)
    .expect("marker should be generated");
  let style = marker_box.style.as_ref();
  assert!(style
    .text_decoration
    .lines
    .contains(TextDecorationLine::UNDERLINE));
  assert_eq!(style.text_decoration.style, TextDecorationStyle::Wavy);
  assert_eq!(style.text_decoration.color, Some(Rgba::BLUE));
  assert_eq!(
    style.text_decoration.thickness,
    TextDecorationThickness::Length(Length::px(2.0))
  );
  assert_eq!(style.text_shadow.len(), 1);
  assert_eq!(style.text_shadow[0].offset_x, Length::px(1.0));
  assert_eq!(style.text_shadow[0].offset_y, Length::px(2.0));
  assert_eq!(style.text_shadow[0].blur_radius, Length::px(3.0));
  assert_eq!(style.text_shadow[0].color, Some(Rgba::GREEN));

  // Layout-affecting properties are reset even when authored on ::marker.
  assert!(style.padding_left.is_zero());
  assert!(style.margin_left.unwrap().is_zero());
  assert_eq!(style.background_color, Rgba::TRANSPARENT);
}

#[test]
fn marker_styles_preserve_text_transform() {
  use crate::style::counters::CounterManager;
  use crate::style::types::CaseTransform;
  use crate::style::types::ListStyleType;

  let mut li_style = ComputedStyle::default();
  li_style.display = Display::ListItem;
  li_style.list_style_type = ListStyleType::String("abc".to_string());

  let mut marker_styles = ComputedStyle::default();
  marker_styles.display = Display::Inline;
  marker_styles.list_style_type = ListStyleType::String("abc".to_string());
  marker_styles.text_transform = TextTransform::with_case(CaseTransform::Uppercase);

  let styled = StyledNode {
    node_id: 0,
    subtree_size: 1,
    node: dom::DomNode {
      node_type: dom::DomNodeType::Element {
        tag_name: "li".to_string(),
        namespace: HTML_NAMESPACE.to_string(),
        attributes: vec![],
      },
      children: vec![],
    },
    styles: Arc::new(li_style),
    starting_styles: StartingStyleSet::default(),
    before_styles: None,
    after_styles: None,
    marker_styles: Some(Arc::new(marker_styles)),
    placeholder_styles: None,
    file_selector_button_styles: None,
    footnote_call_styles: None,
    footnote_marker_styles: None,
    first_line_styles: None,
    first_letter_styles: None,
    slider_thumb_styles: None,
    slider_track_styles: None,
    progress_bar_styles: None,
    progress_value_styles: None,
    meter_bar_styles: None,
    meter_optimum_value_styles: None,
    meter_suboptimum_value_styles: None,
    meter_even_less_good_value_styles: None,
    assigned_slot: None,
    slotted_node_ids: Vec::new(),
    children: vec![],
  };

  let mut quote_depth = 0usize;
  let marker_box = create_marker_box(&styled, &mut CounterManager::default(), &mut quote_depth)
    .expect("marker should be generated");
  assert!(matches!(marker_box.box_type, BoxType::Marker(_)));
  assert_eq!(
    marker_box.style.text_transform,
    TextTransform::with_case(CaseTransform::Uppercase)
  );
}

#[test]
fn pseudo_content_respects_quotes_property() {
  use crate::dom::DomNodeType;
  use crate::style::content::ContentItem;
  use crate::style::content::ContentValue;

  let mut before_style = ComputedStyle::default();
  before_style.content_value = ContentValue::Items(vec![
    ContentItem::OpenQuote,
    ContentItem::String("hi".to_string()),
  ]);
  before_style.quotes = vec![("«".to_string(), "»".to_string())].into();

  let base_style = ComputedStyle::default();
  let styled = StyledNode {
    node_id: 0,
    subtree_size: 1,
    node: dom::DomNode {
      node_type: DomNodeType::Element {
        tag_name: "div".to_string(),
        namespace: HTML_NAMESPACE.to_string(),
        attributes: vec![],
      },
      children: vec![],
    },
    styles: Arc::new(base_style),
    starting_styles: StartingStyleSet::default(),
    before_styles: Some(Arc::new(before_style)),
    after_styles: None,
    marker_styles: None,
    placeholder_styles: None,
    file_selector_button_styles: None,
    footnote_call_styles: None,
    footnote_marker_styles: None,
    first_line_styles: None,
    first_letter_styles: None,
    slider_thumb_styles: None,
    slider_track_styles: None,
    progress_bar_styles: None,
    progress_value_styles: None,
    meter_bar_styles: None,
    meter_optimum_value_styles: None,
    meter_suboptimum_value_styles: None,
    meter_even_less_good_value_styles: None,
    assigned_slot: None,
    slotted_node_ids: Vec::new(),
    children: vec![],
  };

  let mut counters = CounterManager::new();
  counters.enter_scope();
  let mut quote_depth = 0usize;
  let before_box = create_pseudo_element_box(
    &styled,
    styled.before_styles.as_ref().unwrap(),
    clone_starting_style(&styled.starting_styles.before),
    "before",
    &mut counters,
    &mut quote_depth,
  )
  .expect("before box");
  counters.leave_scope();

  assert_eq!(before_box.children.len(), 1);
  if let BoxType::Text(text) = &before_box.children[0].box_type {
    assert_eq!(text.text, "«hi");
  } else {
    panic!("expected text child");
  }
}

#[test]
fn pseudo_content_supports_attr_counter_and_image() {
  use crate::dom::DomNodeType;
  use crate::style::content::ContentItem;
  use crate::style::content::ContentValue;

  let mut before_style = ComputedStyle::default();
  before_style.content_value = ContentValue::Items(vec![
    ContentItem::Attr {
      name: "data-label".to_string(),
      type_or_unit: None,
      fallback: Some("fallback".to_string()),
    },
    ContentItem::String(": ".to_string()),
    ContentItem::Counter {
      name: "item".to_string(),
      style: Some(CounterStyle::UpperRoman.into()),
    },
    ContentItem::String(" ".to_string()),
    ContentItem::Url(crate::style::types::BackgroundImageUrl::new(
      "icon.png".to_string(),
    )),
  ]);

  let base_style = ComputedStyle::default();
  let styled = StyledNode {
    node_id: 0,
    subtree_size: 1,
    node: dom::DomNode {
      node_type: DomNodeType::Element {
        tag_name: "div".to_string(),
        namespace: HTML_NAMESPACE.to_string(),
        attributes: vec![("data-label".to_string(), "hello".to_string())],
      },
      children: vec![],
    },
    styles: Arc::new(base_style),
    starting_styles: StartingStyleSet::default(),
    before_styles: Some(Arc::new(before_style)),
    after_styles: None,
    marker_styles: None,
    placeholder_styles: None,
    file_selector_button_styles: None,
    footnote_call_styles: None,
    footnote_marker_styles: None,
    first_line_styles: None,
    first_letter_styles: None,
    slider_thumb_styles: None,
    slider_track_styles: None,
    progress_bar_styles: None,
    progress_value_styles: None,
    meter_bar_styles: None,
    meter_optimum_value_styles: None,
    meter_suboptimum_value_styles: None,
    meter_even_less_good_value_styles: None,
    assigned_slot: None,
    slotted_node_ids: Vec::new(),
    children: vec![],
  };

  let mut counters = CounterManager::new();
  counters.enter_scope();
  counters.apply_reset(&CounterSet::single("item", 3));

  let mut quote_depth = 0usize;
  let before_box = create_pseudo_element_box(
    &styled,
    styled.before_styles.as_ref().unwrap(),
    clone_starting_style(&styled.starting_styles.before),
    "before",
    &mut counters,
    &mut quote_depth,
  )
  .expect("before box");
  counters.leave_scope();

  // Expect inline container with text + image children
  assert_eq!(before_box.children.len(), 2);
  if let BoxType::Text(text) = &before_box.children[0].box_type {
    assert_eq!(text.text, "hello: III ");
  } else {
    panic!("expected text child");
  }
  if let BoxType::Replaced(replaced) = &before_box.children[1].box_type {
    match &replaced.replaced_type {
      ReplacedType::Image { src, .. } => assert_eq!(src, "icon.png"),
      _ => panic!("expected image replaced content"),
    }
  } else {
    panic!("expected replaced child");
  }
}

#[test]
fn pseudo_content_url_does_not_treat_nbsp_as_empty() {
  use crate::dom::DomNodeType;
  use crate::style::content::ContentItem;
  use crate::style::content::ContentValue;

  let mut before_style = ComputedStyle::default();
  before_style.content_value = ContentValue::Items(vec![ContentItem::Url(
    crate::style::types::BackgroundImageUrl::new("\u{00A0}".to_string()),
  )]);

  let base_style = ComputedStyle::default();
  let styled = StyledNode {
    node_id: 0,
    subtree_size: 1,
    node: dom::DomNode {
      node_type: DomNodeType::Element {
        tag_name: "div".to_string(),
        namespace: HTML_NAMESPACE.to_string(),
        attributes: vec![],
      },
      children: vec![],
    },
    styles: Arc::new(base_style),
    starting_styles: StartingStyleSet::default(),
    before_styles: Some(Arc::new(before_style)),
    after_styles: None,
    marker_styles: None,
    placeholder_styles: None,
    file_selector_button_styles: None,
    footnote_call_styles: None,
    footnote_marker_styles: None,
    first_line_styles: None,
    first_letter_styles: None,
    slider_thumb_styles: None,
    slider_track_styles: None,
    progress_bar_styles: None,
    progress_value_styles: None,
    meter_bar_styles: None,
    meter_optimum_value_styles: None,
    meter_suboptimum_value_styles: None,
    meter_even_less_good_value_styles: None,
    assigned_slot: None,
    slotted_node_ids: Vec::new(),
    children: vec![],
  };

  let mut counters = CounterManager::new();
  counters.enter_scope();
  let mut quote_depth = 0usize;
  let before_box = create_pseudo_element_box(
    &styled,
    styled.before_styles.as_ref().unwrap(),
    clone_starting_style(&styled.starting_styles.before),
    "before",
    &mut counters,
    &mut quote_depth,
  )
  .expect("before box");
  counters.leave_scope();

  assert_eq!(before_box.children.len(), 1);
  if let BoxType::Replaced(replaced) = &before_box.children[0].box_type {
    match &replaced.replaced_type {
      ReplacedType::Image { src, .. } => assert_eq!(src, "\u{00A0}"),
      _ => panic!("expected image replaced content"),
    }
  } else {
    panic!("expected replaced child");
  }
}

#[test]
fn marker_content_url_does_not_treat_nbsp_as_empty() {
  let mut li_style = ComputedStyle::default();
  li_style.display = Display::ListItem;

  let mut marker_style = ComputedStyle::default();
  marker_style.content_value = ContentValue::Items(vec![ContentItem::Url(
    crate::style::types::BackgroundImageUrl::new("\u{00A0}".to_string()),
  )]);

  let styled = StyledNode {
    node_id: 0,
    subtree_size: 1,
    node: dom::DomNode {
      node_type: DomNodeType::Element {
        tag_name: "li".to_string(),
        namespace: HTML_NAMESPACE.to_string(),
        attributes: vec![],
      },
      children: vec![],
    },
    styles: Arc::new(li_style),
    starting_styles: StartingStyleSet::default(),
    before_styles: None,
    after_styles: None,
    marker_styles: Some(Arc::new(marker_style)),
    placeholder_styles: None,
    file_selector_button_styles: None,
    footnote_call_styles: None,
    footnote_marker_styles: None,
    first_line_styles: None,
    first_letter_styles: None,
    slider_thumb_styles: None,
    slider_track_styles: None,
    progress_bar_styles: None,
    progress_value_styles: None,
    meter_bar_styles: None,
    meter_optimum_value_styles: None,
    meter_suboptimum_value_styles: None,
    meter_even_less_good_value_styles: None,
    assigned_slot: None,
    slotted_node_ids: Vec::new(),
    children: vec![],
  };

  let mut quote_depth = 0usize;
  let marker_box = create_marker_box(&styled, &mut CounterManager::default(), &mut quote_depth)
    .expect("marker box");
  let BoxType::Marker(marker) = &marker_box.box_type else {
    panic!("expected marker box");
  };
  match &marker.content {
    MarkerContent::Image(replaced) => match &replaced.replaced_type {
      ReplacedType::Image { src, .. } => assert_eq!(src, "\u{00A0}"),
      other => panic!("expected image marker content, got {other:?}"),
    },
    other => panic!("expected image marker content, got {other:?}"),
  }
}

fn before_pseudo_text(node: &BoxNode) -> String {
  let before = node
    .children
    .iter()
    .find(|child| child.generated_pseudo == Some(GeneratedPseudoElement::Before))
    .expect("expected ::before box");
  before
    .children
    .iter()
    .filter_map(|child| child.text())
    .collect()
}

#[test]
fn before_counter_increment_affects_element_children() {
  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Block;
  container_style.counters.counter_reset = Some(CounterSet::single("x", 0));

  let mut container_before_style = ComputedStyle::default();
  container_before_style.content_value =
    ContentValue::Items(vec![ContentItem::String(String::new())]);
  container_before_style.counters.counter_increment = Some(CounterSet::single("x", 1));

  let mut span_before_style = ComputedStyle::default();
  span_before_style.content_value = ContentValue::Items(vec![ContentItem::Counter {
    name: "x".to_string(),
    style: None,
  }]);
  let span_before_style = Arc::new(span_before_style);

  let mut span_a = styled_element("span");
  span_a.node_id = 1;
  span_a.before_styles = Some(Arc::clone(&span_before_style));

  let mut span_b = styled_element("span");
  span_b.node_id = 2;
  span_b.before_styles = Some(span_before_style);

  let mut container = styled_element("div");
  container.node_id = 0;
  container.styles = Arc::new(container_style);
  container.before_styles = Some(Arc::new(container_before_style));
  container.children = vec![span_a, span_b];

  let tree = generate_box_tree(&container);
  assert_eq!(tree.root.children.len(), 3);
  assert_eq!(before_pseudo_text(&tree.root.children[1]), "1");
  assert_eq!(before_pseudo_text(&tree.root.children[2]), "1");
}

#[test]
fn after_counter_increment_happens_after_children() {
  let mut root_style = ComputedStyle::default();
  root_style.display = Display::Block;
  root_style.counters.counter_reset = Some(CounterSet::single("x", 0));

  let mut container_after_style = ComputedStyle::default();
  container_after_style.content_value =
    ContentValue::Items(vec![ContentItem::String(String::new())]);
  container_after_style.counters.counter_increment = Some(CounterSet::single("x", 1));

  let mut before_counter_style = ComputedStyle::default();
  before_counter_style.content_value = ContentValue::Items(vec![ContentItem::Counter {
    name: "x".to_string(),
    style: None,
  }]);
  let before_counter_style = Arc::new(before_counter_style);

  let mut span_a = styled_element("span");
  span_a.node_id = 2;
  span_a.before_styles = Some(Arc::clone(&before_counter_style));

  let mut span_b = styled_element("span");
  span_b.node_id = 3;
  span_b.before_styles = Some(Arc::clone(&before_counter_style));

  let mut container = styled_element("div");
  container.node_id = 1;
  container.styles = Arc::new({
    let mut style = ComputedStyle::default();
    style.display = Display::Block;
    style
  });
  container.after_styles = Some(Arc::new(container_after_style));
  container.children = vec![span_a, span_b];

  let mut sibling = styled_element("div");
  sibling.node_id = 4;
  sibling.styles = Arc::new({
    let mut style = ComputedStyle::default();
    style.display = Display::Block;
    style
  });
  sibling.before_styles = Some(before_counter_style);

  let mut root = styled_element("div");
  root.node_id = 0;
  root.styles = Arc::new(root_style);
  root.children = vec![container, sibling];

  let tree = generate_box_tree(&root);
  assert_eq!(tree.root.children.len(), 2);

  let container_box = &tree.root.children[0];
  assert_eq!(before_pseudo_text(&container_box.children[0]), "0");
  assert_eq!(before_pseudo_text(&container_box.children[1]), "0");

  let sibling_box = &tree.root.children[1];
  assert_eq!(before_pseudo_text(sibling_box), "1");
}

#[test]
fn marker_counter_increment_affects_list_item_children() {
  let mut li_style = ComputedStyle::default();
  li_style.display = Display::ListItem;
  li_style.counters.counter_reset = Some(CounterSet::single("x", 0));

  let mut marker_style = ComputedStyle::default();
  marker_style.content_value = ContentValue::Items(vec![ContentItem::String(String::new())]);
  marker_style.counters.counter_increment = Some(CounterSet::single("x", 1));

  let mut before_style = ComputedStyle::default();
  before_style.content_value = ContentValue::Items(vec![ContentItem::Counter {
    name: "x".to_string(),
    style: None,
  }]);

  let mut li = styled_element("li");
  li.node_id = 0;
  li.styles = Arc::new(li_style);
  li.marker_styles = Some(Arc::new(marker_style));
  li.before_styles = Some(Arc::new(before_style));

  let tree = generate_box_tree(&li);
  assert_eq!(tree.root.children.len(), 2);
  assert!(matches!(tree.root.children[0].box_type, BoxType::Marker(_)));
  assert_eq!(before_pseudo_text(&tree.root), "1");
}

#[test]
fn before_counter_increment_affects_descendant_generated_content() {
  use crate::dom::DomNodeType;
  use crate::style::content::ContentItem;
  use crate::style::content::ContentValue;

  let mut root_style = ComputedStyle::default();
  root_style.display = Display::Block;
  root_style.counters.counter_reset = Some(CounterSet::single("section", 0));

  let mut parent_before_style = ComputedStyle::default();
  parent_before_style.content_value = ContentValue::Items(Vec::new());
  parent_before_style.counters.counter_increment = Some(CounterSet::single("section", 1));

  let mut child_before_style = ComputedStyle::default();
  child_before_style.content_value = ContentValue::Items(vec![ContentItem::Counter {
    name: "section".to_string(),
    style: None,
  }]);

  let child = StyledNode {
    node_id: 2,
    subtree_size: 1,
    node: dom::DomNode {
      node_type: DomNodeType::Element {
        tag_name: "span".to_string(),
        namespace: HTML_NAMESPACE.to_string(),
        attributes: vec![],
      },
      children: vec![],
    },
    styles: Arc::new(ComputedStyle::default()),
    starting_styles: StartingStyleSet::default(),
    before_styles: Some(Arc::new(child_before_style)),
    after_styles: None,
    marker_styles: None,
    first_line_styles: None,
    first_letter_styles: None,
    placeholder_styles: None,
    file_selector_button_styles: None,
    footnote_call_styles: None,
    footnote_marker_styles: None,
    slider_thumb_styles: None,
    slider_track_styles: None,
    progress_bar_styles: None,
    progress_value_styles: None,
    meter_bar_styles: None,
    meter_optimum_value_styles: None,
    meter_suboptimum_value_styles: None,
    meter_even_less_good_value_styles: None,
    assigned_slot: None,
    slotted_node_ids: Vec::new(),
    children: vec![],
  };

  let parent = StyledNode {
    node_id: 1,
    subtree_size: 1,
    node: dom::DomNode {
      node_type: DomNodeType::Element {
        tag_name: "div".to_string(),
        namespace: HTML_NAMESPACE.to_string(),
        attributes: vec![],
      },
      children: vec![],
    },
    styles: Arc::new(ComputedStyle::default()),
    starting_styles: StartingStyleSet::default(),
    before_styles: Some(Arc::new(parent_before_style)),
    after_styles: None,
    marker_styles: None,
    first_line_styles: None,
    first_letter_styles: None,
    placeholder_styles: None,
    file_selector_button_styles: None,
    footnote_call_styles: None,
    footnote_marker_styles: None,
    slider_thumb_styles: None,
    slider_track_styles: None,
    progress_bar_styles: None,
    progress_value_styles: None,
    meter_bar_styles: None,
    meter_optimum_value_styles: None,
    meter_suboptimum_value_styles: None,
    meter_even_less_good_value_styles: None,
    assigned_slot: None,
    slotted_node_ids: Vec::new(),
    children: vec![child],
  };

  let root = StyledNode {
    node_id: 0,
    subtree_size: 1,
    node: dom::DomNode {
      node_type: DomNodeType::Element {
        tag_name: "div".to_string(),
        namespace: HTML_NAMESPACE.to_string(),
        attributes: vec![],
      },
      children: vec![],
    },
    styles: Arc::new(root_style),
    starting_styles: StartingStyleSet::default(),
    before_styles: None,
    after_styles: None,
    marker_styles: None,
    first_line_styles: None,
    first_letter_styles: None,
    placeholder_styles: None,
    file_selector_button_styles: None,
    footnote_call_styles: None,
    footnote_marker_styles: None,
    slider_thumb_styles: None,
    slider_track_styles: None,
    progress_bar_styles: None,
    progress_value_styles: None,
    meter_bar_styles: None,
    meter_optimum_value_styles: None,
    meter_suboptimum_value_styles: None,
    meter_even_less_good_value_styles: None,
    assigned_slot: None,
    slotted_node_ids: Vec::new(),
    children: vec![parent],
  };

  let tree = generate_box_tree(&root);
  assert_eq!(
    pseudo_text(&tree.root, 2, GeneratedPseudoElement::Before),
    "1",
    "descendant ::before should see counter increment from ancestor ::before"
  );
}

#[test]
fn display_contents_nodes_affect_counters() {
  use crate::dom::DomNodeType;
  use crate::style::content::ContentItem;
  use crate::style::content::ContentValue;
  let mut root_style = ComputedStyle::default();
  root_style.display = Display::Block;
  root_style.counters.counter_reset = Some(CounterSet::single("foo", 0));

  let mut contents_style = ComputedStyle::default();
  contents_style.display = Display::Contents;
  contents_style.counters.counter_increment = Some(CounterSet::single("foo", 1));

  let mut probe_before_style = ComputedStyle::default();
  probe_before_style.content_value = ContentValue::Items(vec![ContentItem::Counter {
    name: "foo".to_string(),
    style: None,
  }]);

  let contents_node = StyledNode {
    node_id: 1,
    subtree_size: 1,
    node: dom::DomNode {
      node_type: DomNodeType::Element {
        tag_name: "div".to_string(),
        namespace: HTML_NAMESPACE.to_string(),
        attributes: vec![],
      },
      children: vec![],
    },
    styles: Arc::new(contents_style),
    starting_styles: StartingStyleSet::default(),
    before_styles: None,
    after_styles: None,
    marker_styles: None,
    first_line_styles: None,
    first_letter_styles: None,
    placeholder_styles: None,
    file_selector_button_styles: None,
    footnote_call_styles: None,
    footnote_marker_styles: None,
    slider_thumb_styles: None,
    slider_track_styles: None,
    progress_bar_styles: None,
    progress_value_styles: None,
    meter_bar_styles: None,
    meter_optimum_value_styles: None,
    meter_suboptimum_value_styles: None,
    meter_even_less_good_value_styles: None,
    assigned_slot: None,
    slotted_node_ids: Vec::new(),
    children: vec![],
  };

  let probe_node = StyledNode {
    node_id: 2,
    subtree_size: 1,
    node: dom::DomNode {
      node_type: DomNodeType::Element {
        tag_name: "div".to_string(),
        namespace: HTML_NAMESPACE.to_string(),
        attributes: vec![],
      },
      children: vec![],
    },
    styles: Arc::new(ComputedStyle::default()),
    starting_styles: StartingStyleSet::default(),
    before_styles: Some(Arc::new(probe_before_style)),
    after_styles: None,
    marker_styles: None,
    first_line_styles: None,
    first_letter_styles: None,
    placeholder_styles: None,
    file_selector_button_styles: None,
    footnote_call_styles: None,
    footnote_marker_styles: None,
    slider_thumb_styles: None,
    slider_track_styles: None,
    progress_bar_styles: None,
    progress_value_styles: None,
    meter_bar_styles: None,
    meter_optimum_value_styles: None,
    meter_suboptimum_value_styles: None,
    meter_even_less_good_value_styles: None,
    assigned_slot: None,
    slotted_node_ids: Vec::new(),
    children: vec![],
  };

  let root = StyledNode {
    node_id: 0,
    subtree_size: 1,
    node: dom::DomNode {
      node_type: DomNodeType::Element {
        tag_name: "div".to_string(),
        namespace: HTML_NAMESPACE.to_string(),
        attributes: vec![],
      },
      children: vec![],
    },
    styles: Arc::new(root_style),
    starting_styles: StartingStyleSet::default(),
    before_styles: None,
    after_styles: None,
    marker_styles: None,
    first_line_styles: None,
    first_letter_styles: None,
    placeholder_styles: None,
    file_selector_button_styles: None,
    footnote_call_styles: None,
    footnote_marker_styles: None,
    slider_thumb_styles: None,
    slider_track_styles: None,
    progress_bar_styles: None,
    progress_value_styles: None,
    meter_bar_styles: None,
    meter_optimum_value_styles: None,
    meter_suboptimum_value_styles: None,
    meter_even_less_good_value_styles: None,
    assigned_slot: None,
    slotted_node_ids: Vec::new(),
    children: vec![contents_node, probe_node],
  };

  let tree = generate_box_tree(&root);
  assert_eq!(
    pseudo_text(&tree.root, 2, GeneratedPseudoElement::Before),
    "1",
    "display:contents elements must still apply counter-increment"
  );
}

#[test]
fn pseudo_element_content_none_does_not_affect_counters() {
  use crate::dom::DomNodeType;
  use crate::style::content::ContentItem;
  use crate::style::content::ContentValue;

  let mut root_style = ComputedStyle::default();
  root_style.display = Display::Block;
  root_style.counters.counter_reset = Some(CounterSet::single("foo", 0));

  let mut suppressed_before_style = ComputedStyle::default();
  suppressed_before_style.content_value = ContentValue::None;
  suppressed_before_style.counters.counter_increment = Some(CounterSet::single("foo", 1));

  let mut child_before_style = ComputedStyle::default();
  child_before_style.content_value = ContentValue::Items(vec![ContentItem::Counter {
    name: "foo".to_string(),
    style: None,
  }]);

  let child = StyledNode {
    node_id: 2,
    subtree_size: 1,
    node: dom::DomNode {
      node_type: DomNodeType::Element {
        tag_name: "span".to_string(),
        namespace: HTML_NAMESPACE.to_string(),
        attributes: vec![],
      },
      children: vec![],
    },
    styles: Arc::new(ComputedStyle::default()),
    starting_styles: StartingStyleSet::default(),
    before_styles: Some(Arc::new(child_before_style)),
    after_styles: None,
    marker_styles: None,
    first_line_styles: None,
    first_letter_styles: None,
    placeholder_styles: None,
    file_selector_button_styles: None,
    footnote_call_styles: None,
    footnote_marker_styles: None,
    slider_thumb_styles: None,
    slider_track_styles: None,
    progress_bar_styles: None,
    progress_value_styles: None,
    meter_bar_styles: None,
    meter_optimum_value_styles: None,
    meter_suboptimum_value_styles: None,
    meter_even_less_good_value_styles: None,
    assigned_slot: None,
    slotted_node_ids: Vec::new(),
    children: vec![],
  };

  let parent = StyledNode {
    node_id: 1,
    subtree_size: 1,
    node: dom::DomNode {
      node_type: DomNodeType::Element {
        tag_name: "div".to_string(),
        namespace: HTML_NAMESPACE.to_string(),
        attributes: vec![],
      },
      children: vec![],
    },
    styles: Arc::new(ComputedStyle::default()),
    starting_styles: StartingStyleSet::default(),
    before_styles: Some(Arc::new(suppressed_before_style)),
    after_styles: None,
    marker_styles: None,
    first_line_styles: None,
    first_letter_styles: None,
    placeholder_styles: None,
    file_selector_button_styles: None,
    footnote_call_styles: None,
    footnote_marker_styles: None,
    slider_thumb_styles: None,
    slider_track_styles: None,
    progress_bar_styles: None,
    progress_value_styles: None,
    meter_bar_styles: None,
    meter_optimum_value_styles: None,
    meter_suboptimum_value_styles: None,
    meter_even_less_good_value_styles: None,
    assigned_slot: None,
    slotted_node_ids: Vec::new(),
    children: vec![child],
  };

  let root = StyledNode {
    node_id: 0,
    subtree_size: 1,
    node: dom::DomNode {
      node_type: DomNodeType::Element {
        tag_name: "div".to_string(),
        namespace: HTML_NAMESPACE.to_string(),
        attributes: vec![],
      },
      children: vec![],
    },
    styles: Arc::new(root_style),
    starting_styles: StartingStyleSet::default(),
    before_styles: None,
    after_styles: None,
    marker_styles: None,
    first_line_styles: None,
    first_letter_styles: None,
    placeholder_styles: None,
    file_selector_button_styles: None,
    footnote_call_styles: None,
    footnote_marker_styles: None,
    slider_thumb_styles: None,
    slider_track_styles: None,
    progress_bar_styles: None,
    progress_value_styles: None,
    meter_bar_styles: None,
    meter_optimum_value_styles: None,
    meter_suboptimum_value_styles: None,
    meter_even_less_good_value_styles: None,
    assigned_slot: None,
    slotted_node_ids: Vec::new(),
    children: vec![parent],
  };

  let tree = generate_box_tree(&root);
  assert_eq!(
    pseudo_text(&tree.root, 2, GeneratedPseudoElement::Before),
    "0",
    "::before with content:none must not apply counter-increment"
  );
}

#[test]
fn marker_content_none_does_not_affect_counters() {
  use crate::dom::DomNodeType;
  use crate::style::content::ContentItem;
  use crate::style::content::ContentValue;
  use crate::style::types::ListStyleType;

  let mut root_style = ComputedStyle::default();
  root_style.display = Display::Block;
  root_style.counters.counter_reset = Some(CounterSet::single("foo", 0));

  let mut li_style = ComputedStyle::default();
  li_style.display = Display::ListItem;
  li_style.list_style_type = ListStyleType::Decimal;

  let mut marker_style = ComputedStyle::default();
  marker_style.content_value = ContentValue::None;
  marker_style.counters.counter_increment = Some(CounterSet::single("foo", 1));

  let mut child_before_style = ComputedStyle::default();
  child_before_style.content_value = ContentValue::Items(vec![ContentItem::Counter {
    name: "foo".to_string(),
    style: None,
  }]);

  let child = StyledNode {
    node_id: 2,
    subtree_size: 1,
    node: dom::DomNode {
      node_type: DomNodeType::Element {
        tag_name: "span".to_string(),
        namespace: HTML_NAMESPACE.to_string(),
        attributes: vec![],
      },
      children: vec![],
    },
    styles: Arc::new(ComputedStyle::default()),
    starting_styles: StartingStyleSet::default(),
    before_styles: Some(Arc::new(child_before_style)),
    after_styles: None,
    marker_styles: None,
    first_line_styles: None,
    first_letter_styles: None,
    placeholder_styles: None,
    file_selector_button_styles: None,
    footnote_call_styles: None,
    footnote_marker_styles: None,
    slider_thumb_styles: None,
    slider_track_styles: None,
    progress_bar_styles: None,
    progress_value_styles: None,
    meter_bar_styles: None,
    meter_optimum_value_styles: None,
    meter_suboptimum_value_styles: None,
    meter_even_less_good_value_styles: None,
    assigned_slot: None,
    slotted_node_ids: Vec::new(),
    children: vec![],
  };

  let li = StyledNode {
    node_id: 1,
    subtree_size: 1,
    node: dom::DomNode {
      node_type: DomNodeType::Element {
        tag_name: "li".to_string(),
        namespace: HTML_NAMESPACE.to_string(),
        attributes: vec![],
      },
      children: vec![],
    },
    styles: Arc::new(li_style),
    starting_styles: StartingStyleSet::default(),
    before_styles: None,
    after_styles: None,
    marker_styles: Some(Arc::new(marker_style)),
    first_line_styles: None,
    first_letter_styles: None,
    placeholder_styles: None,
    file_selector_button_styles: None,
    footnote_call_styles: None,
    footnote_marker_styles: None,
    slider_thumb_styles: None,
    slider_track_styles: None,
    progress_bar_styles: None,
    progress_value_styles: None,
    meter_bar_styles: None,
    meter_optimum_value_styles: None,
    meter_suboptimum_value_styles: None,
    meter_even_less_good_value_styles: None,
    assigned_slot: None,
    slotted_node_ids: Vec::new(),
    children: vec![child],
  };

  let root = StyledNode {
    node_id: 0,
    subtree_size: 1,
    node: dom::DomNode {
      node_type: DomNodeType::Element {
        tag_name: "div".to_string(),
        namespace: HTML_NAMESPACE.to_string(),
        attributes: vec![],
      },
      children: vec![],
    },
    styles: Arc::new(root_style),
    starting_styles: StartingStyleSet::default(),
    before_styles: None,
    after_styles: None,
    marker_styles: None,
    first_line_styles: None,
    first_letter_styles: None,
    placeholder_styles: None,
    file_selector_button_styles: None,
    footnote_call_styles: None,
    footnote_marker_styles: None,
    slider_thumb_styles: None,
    slider_track_styles: None,
    progress_bar_styles: None,
    progress_value_styles: None,
    meter_bar_styles: None,
    meter_optimum_value_styles: None,
    meter_suboptimum_value_styles: None,
    meter_even_less_good_value_styles: None,
    assigned_slot: None,
    slotted_node_ids: Vec::new(),
    children: vec![li],
  };

  let tree = generate_box_tree(&root);
  assert_eq!(
    pseudo_text(&tree.root, 2, GeneratedPseudoElement::Before),
    "0",
    "::marker with content:none must not apply counter-increment"
  );
}

#[test]
fn marker_counter_increment_affects_list_item_descendants() {
  use crate::dom::DomNodeType;
  use crate::style::content::ContentItem;
  use crate::style::content::ContentValue;
  use crate::style::types::ListStyleType;

  let mut root_style = ComputedStyle::default();
  root_style.display = Display::Block;
  root_style.counters.counter_reset = Some(CounterSet::single("section", 0));

  let mut li_style = ComputedStyle::default();
  li_style.display = Display::ListItem;
  li_style.list_style_type = ListStyleType::Decimal;

  let mut marker_style = ComputedStyle::default();
  marker_style.display = Display::Inline;
  marker_style.content_value = ContentValue::Items(vec![ContentItem::String("M".to_string())]);
  marker_style.counters.counter_increment = Some(CounterSet::single("section", 1));

  let mut child_before_style = ComputedStyle::default();
  child_before_style.content_value = ContentValue::Items(vec![ContentItem::Counter {
    name: "section".to_string(),
    style: None,
  }]);

  let child = StyledNode {
    node_id: 2,
    subtree_size: 1,
    node: dom::DomNode {
      node_type: DomNodeType::Element {
        tag_name: "span".to_string(),
        namespace: HTML_NAMESPACE.to_string(),
        attributes: vec![],
      },
      children: vec![],
    },
    styles: Arc::new(ComputedStyle::default()),
    starting_styles: StartingStyleSet::default(),
    before_styles: Some(Arc::new(child_before_style)),
    after_styles: None,
    marker_styles: None,
    first_line_styles: None,
    first_letter_styles: None,
    placeholder_styles: None,
    file_selector_button_styles: None,
    footnote_call_styles: None,
    footnote_marker_styles: None,
    slider_thumb_styles: None,
    slider_track_styles: None,
    progress_bar_styles: None,
    progress_value_styles: None,
    meter_bar_styles: None,
    meter_optimum_value_styles: None,
    meter_suboptimum_value_styles: None,
    meter_even_less_good_value_styles: None,
    assigned_slot: None,
    slotted_node_ids: Vec::new(),
    children: vec![],
  };

  let li = StyledNode {
    node_id: 1,
    subtree_size: 1,
    node: dom::DomNode {
      node_type: DomNodeType::Element {
        tag_name: "li".to_string(),
        namespace: HTML_NAMESPACE.to_string(),
        attributes: vec![],
      },
      children: vec![],
    },
    styles: Arc::new(li_style),
    starting_styles: StartingStyleSet::default(),
    before_styles: None,
    after_styles: None,
    marker_styles: Some(Arc::new(marker_style)),
    first_line_styles: None,
    first_letter_styles: None,
    placeholder_styles: None,
    file_selector_button_styles: None,
    footnote_call_styles: None,
    footnote_marker_styles: None,
    slider_thumb_styles: None,
    slider_track_styles: None,
    progress_bar_styles: None,
    progress_value_styles: None,
    meter_bar_styles: None,
    meter_optimum_value_styles: None,
    meter_suboptimum_value_styles: None,
    meter_even_less_good_value_styles: None,
    assigned_slot: None,
    slotted_node_ids: Vec::new(),
    children: vec![child],
  };

  let root = StyledNode {
    node_id: 0,
    subtree_size: 1,
    node: dom::DomNode {
      node_type: DomNodeType::Element {
        tag_name: "div".to_string(),
        namespace: HTML_NAMESPACE.to_string(),
        attributes: vec![],
      },
      children: vec![],
    },
    styles: Arc::new(root_style),
    starting_styles: StartingStyleSet::default(),
    before_styles: None,
    after_styles: None,
    marker_styles: None,
    first_line_styles: None,
    first_letter_styles: None,
    placeholder_styles: None,
    file_selector_button_styles: None,
    footnote_call_styles: None,
    footnote_marker_styles: None,
    slider_thumb_styles: None,
    slider_track_styles: None,
    progress_bar_styles: None,
    progress_value_styles: None,
    meter_bar_styles: None,
    meter_optimum_value_styles: None,
    meter_suboptimum_value_styles: None,
    meter_even_less_good_value_styles: None,
    assigned_slot: None,
    slotted_node_ids: Vec::new(),
    children: vec![li],
  };

  let tree = generate_box_tree(&root);
  assert_eq!(
    pseudo_text(&tree.root, 2, GeneratedPseudoElement::Before),
    "1",
    "list item descendant should see counter increment from ::marker"
  );
}

#[test]
fn after_counter_increment_occurs_after_descendants() {
  use crate::dom::DomNodeType;
  use crate::style::content::ContentItem;
  use crate::style::content::ContentValue;

  let mut root_style = ComputedStyle::default();
  root_style.display = Display::Block;
  root_style.counters.counter_reset = Some(CounterSet::single("section", 0));

  let mut child_before_style = ComputedStyle::default();
  child_before_style.content_value = ContentValue::Items(vec![ContentItem::Counter {
    name: "section".to_string(),
    style: None,
  }]);

  let mk_counter_span = |node_id: usize| StyledNode {
    node_id,
    subtree_size: 1,
    node: dom::DomNode {
      node_type: DomNodeType::Element {
        tag_name: "span".to_string(),
        namespace: HTML_NAMESPACE.to_string(),
        attributes: vec![],
      },
      children: vec![],
    },
    styles: Arc::new(ComputedStyle::default()),
    starting_styles: StartingStyleSet::default(),
    before_styles: Some(Arc::new(child_before_style.clone())),
    after_styles: None,
    marker_styles: None,
    first_line_styles: None,
    first_letter_styles: None,
    placeholder_styles: None,
    file_selector_button_styles: None,
    footnote_call_styles: None,
    footnote_marker_styles: None,
    slider_thumb_styles: None,
    slider_track_styles: None,
    progress_bar_styles: None,
    progress_value_styles: None,
    meter_bar_styles: None,
    meter_optimum_value_styles: None,
    meter_suboptimum_value_styles: None,
    meter_even_less_good_value_styles: None,
    assigned_slot: None,
    slotted_node_ids: Vec::new(),
    children: vec![],
  };

  let mut after_style = ComputedStyle::default();
  after_style.content_value = ContentValue::Items(Vec::new());
  after_style.counters.counter_increment = Some(CounterSet::single("section", 1));

  let target = StyledNode {
    node_id: 1,
    subtree_size: 1,
    node: dom::DomNode {
      node_type: DomNodeType::Element {
        tag_name: "div".to_string(),
        namespace: HTML_NAMESPACE.to_string(),
        attributes: vec![],
      },
      children: vec![],
    },
    styles: Arc::new(ComputedStyle::default()),
    starting_styles: StartingStyleSet::default(),
    before_styles: None,
    after_styles: Some(Arc::new(after_style)),
    marker_styles: None,
    first_line_styles: None,
    first_letter_styles: None,
    placeholder_styles: None,
    file_selector_button_styles: None,
    footnote_call_styles: None,
    footnote_marker_styles: None,
    slider_thumb_styles: None,
    slider_track_styles: None,
    progress_bar_styles: None,
    progress_value_styles: None,
    meter_bar_styles: None,
    meter_optimum_value_styles: None,
    meter_suboptimum_value_styles: None,
    meter_even_less_good_value_styles: None,
    assigned_slot: None,
    slotted_node_ids: Vec::new(),
    children: vec![mk_counter_span(2)],
  };

  let sibling = StyledNode {
    node_id: 3,
    subtree_size: 1,
    node: dom::DomNode {
      node_type: DomNodeType::Element {
        tag_name: "p".to_string(),
        namespace: HTML_NAMESPACE.to_string(),
        attributes: vec![],
      },
      children: vec![],
    },
    styles: Arc::new(ComputedStyle::default()),
    starting_styles: StartingStyleSet::default(),
    before_styles: None,
    after_styles: None,
    marker_styles: None,
    first_line_styles: None,
    first_letter_styles: None,
    placeholder_styles: None,
    file_selector_button_styles: None,
    footnote_call_styles: None,
    footnote_marker_styles: None,
    slider_thumb_styles: None,
    slider_track_styles: None,
    progress_bar_styles: None,
    progress_value_styles: None,
    meter_bar_styles: None,
    meter_optimum_value_styles: None,
    meter_suboptimum_value_styles: None,
    meter_even_less_good_value_styles: None,
    assigned_slot: None,
    slotted_node_ids: Vec::new(),
    children: vec![mk_counter_span(4)],
  };

  let root = StyledNode {
    node_id: 0,
    subtree_size: 1,
    node: dom::DomNode {
      node_type: DomNodeType::Element {
        tag_name: "div".to_string(),
        namespace: HTML_NAMESPACE.to_string(),
        attributes: vec![],
      },
      children: vec![],
    },
    styles: Arc::new(root_style),
    starting_styles: StartingStyleSet::default(),
    before_styles: None,
    after_styles: None,
    marker_styles: None,
    first_line_styles: None,
    first_letter_styles: None,
    placeholder_styles: None,
    file_selector_button_styles: None,
    footnote_call_styles: None,
    footnote_marker_styles: None,
    slider_thumb_styles: None,
    slider_track_styles: None,
    progress_bar_styles: None,
    progress_value_styles: None,
    meter_bar_styles: None,
    meter_optimum_value_styles: None,
    meter_suboptimum_value_styles: None,
    meter_even_less_good_value_styles: None,
    assigned_slot: None,
    slotted_node_ids: Vec::new(),
    children: vec![target, sibling],
  };

  let tree = generate_box_tree(&root);
  assert_eq!(
    pseudo_text(&tree.root, 2, GeneratedPseudoElement::Before),
    "0",
    "descendants should observe counter before ::after increments run"
  );
  assert_eq!(
    pseudo_text(&tree.root, 4, GeneratedPseudoElement::Before),
    "1",
    "following siblings should observe counter after ::after increments run"
  );
}

#[test]
fn marker_content_resolves_structured_content() {
  use crate::dom::DomNodeType;
  use crate::style::content::ContentItem;
  use crate::style::content::ContentValue;

  let mut marker_style = ComputedStyle::default();
  marker_style.content_value = ContentValue::Items(vec![
    ContentItem::String("[".to_string()),
    ContentItem::Counter {
      name: "item".to_string(),
      style: Some(CounterStyle::LowerRoman.into()),
    },
    ContentItem::String("]".to_string()),
  ]);

  let base_style = ComputedStyle::default();
  let styled = StyledNode {
    node_id: 0,
    subtree_size: 1,
    node: dom::DomNode {
      node_type: DomNodeType::Element {
        tag_name: "li".to_string(),
        namespace: HTML_NAMESPACE.to_string(),
        attributes: vec![],
      },
      children: vec![],
    },
    styles: Arc::new(base_style),
    starting_styles: StartingStyleSet::default(),
    before_styles: None,
    after_styles: None,
    marker_styles: Some(Arc::new(marker_style)),
    placeholder_styles: None,
    file_selector_button_styles: None,
    footnote_call_styles: None,
    footnote_marker_styles: None,
    first_line_styles: None,
    first_letter_styles: None,
    slider_thumb_styles: None,
    slider_track_styles: None,
    progress_bar_styles: None,
    progress_value_styles: None,
    meter_bar_styles: None,
    meter_optimum_value_styles: None,
    meter_suboptimum_value_styles: None,
    meter_even_less_good_value_styles: None,
    assigned_slot: None,
    slotted_node_ids: Vec::new(),
    children: vec![],
  };

  let mut counters = CounterManager::new();
  counters.enter_scope();
  counters.apply_reset(&CounterSet::single("item", 2));
  counters.apply_increment(&CounterSet::single("item", 1));

  let mut quote_depth = 0usize;
  let marker_box = create_marker_box(&styled, &mut counters, &mut quote_depth).expect("marker");
  counters.leave_scope();

  match &marker_box.box_type {
    BoxType::Marker(marker) => match &marker.content {
      MarkerContent::Text(t) => assert_eq!(t.as_str(), "[iii]"),
      MarkerContent::Image(_) => panic!("expected text marker"),
    },
    _ => panic!("expected marker box"),
  }
}

#[test]
fn marker_uses_string_list_style_type() {
  let mut style = ComputedStyle::default();
  style.list_style_type = ListStyleType::String("★".to_string());
  style.display = Display::ListItem;
  let style = Arc::new(style);
  let styled = StyledNode {
    node_id: 0,
    subtree_size: 1,
    node: dom::DomNode {
      node_type: dom::DomNodeType::Element {
        tag_name: "li".to_string(),
        namespace: HTML_NAMESPACE.to_string(),
        attributes: vec![],
      },
      children: vec![],
    },
    styles: style.clone(),
    marker_styles: Some(style.clone()),
    placeholder_styles: None,
    file_selector_button_styles: None,
    footnote_call_styles: None,
    footnote_marker_styles: None,
    starting_styles: StartingStyleSet::default(),
    before_styles: None,
    after_styles: None,
    first_line_styles: None,
    first_letter_styles: None,
    slider_thumb_styles: None,
    slider_track_styles: None,
    progress_bar_styles: None,
    progress_value_styles: None,
    meter_bar_styles: None,
    meter_optimum_value_styles: None,
    meter_suboptimum_value_styles: None,
    meter_even_less_good_value_styles: None,
    assigned_slot: None,
    slotted_node_ids: Vec::new(),
    children: vec![],
  };

  let mut counters = CounterManager::new();
  counters.enter_scope();
  counters.apply_reset(&CounterSet::single("list-item", 1));

  let mut quote_depth = 0usize;
  let marker_box = create_marker_box(&styled, &mut counters, &mut quote_depth).expect("marker");
  counters.leave_scope();

  match &marker_box.box_type {
    BoxType::Marker(marker) => match &marker.content {
      MarkerContent::Text(t) => assert_eq!(t.as_str(), "★"),
      MarkerContent::Image(_) => panic!("expected text marker from string list-style-type"),
    },
    _ => panic!("expected marker box"),
  }
}

#[test]
fn marker_uses_symbols_list_style_type() {
  let mut style = ComputedStyle::default();
  style.display = Display::ListItem;
  style.list_style_type = ListStyleType::Symbols(SymbolsCounterStyle {
    system: SymbolsType::Symbolic,
    symbols: vec!["*".to_string(), "†".to_string()],
  });
  let style = Arc::new(style);

  let styled = StyledNode {
    node_id: 0,
    subtree_size: 1,
    node: dom::DomNode {
      node_type: dom::DomNodeType::Element {
        tag_name: "li".to_string(),
        namespace: HTML_NAMESPACE.to_string(),
        attributes: vec![],
      },
      children: vec![],
    },
    styles: style.clone(),
    marker_styles: Some(style.clone()),
    placeholder_styles: None,
    file_selector_button_styles: None,
    footnote_call_styles: None,
    footnote_marker_styles: None,
    starting_styles: StartingStyleSet::default(),
    before_styles: None,
    after_styles: None,
    first_line_styles: None,
    first_letter_styles: None,
    slider_thumb_styles: None,
    slider_track_styles: None,
    progress_bar_styles: None,
    progress_value_styles: None,
    meter_bar_styles: None,
    meter_optimum_value_styles: None,
    meter_suboptimum_value_styles: None,
    meter_even_less_good_value_styles: None,
    assigned_slot: None,
    slotted_node_ids: Vec::new(),
    children: vec![],
  };

  let mut counters = CounterManager::new();
  counters.enter_scope();
  counters.apply_reset(&CounterSet::single("list-item", 3));

  let mut quote_depth = 0usize;
  let marker_box = create_marker_box(&styled, &mut counters, &mut quote_depth).expect("marker");
  counters.leave_scope();

  match &marker_box.box_type {
    BoxType::Marker(marker) => match &marker.content {
      MarkerContent::Text(t) => assert_eq!(t.as_str(), "** "),
      MarkerContent::Image(_) => panic!("expected text marker from symbols() list-style-type"),
    },
    _ => panic!("expected marker box"),
  }
}

#[test]
fn disclosure_closed_marker_points_right_in_ltr() {
  let mut style = ComputedStyle::default();
  style.display = Display::ListItem;
  style.list_style_type = ListStyleType::DisclosureClosed;
  style.direction = Direction::Ltr;
  style.writing_mode = WritingMode::HorizontalTb;
  let style = Arc::new(style);

  let styled = StyledNode {
    node_id: 0,
    subtree_size: 1,
    node: dom::DomNode {
      node_type: dom::DomNodeType::Element {
        tag_name: "li".to_string(),
        namespace: HTML_NAMESPACE.to_string(),
        attributes: vec![],
      },
      children: vec![],
    },
    styles: style.clone(),
    marker_styles: None,
    starting_styles: StartingStyleSet::default(),
    before_styles: None,
    after_styles: None,
    first_line_styles: None,
    first_letter_styles: None,
    placeholder_styles: None,
    file_selector_button_styles: None,
    footnote_call_styles: None,
    footnote_marker_styles: None,
    slider_thumb_styles: None,
    slider_track_styles: None,
    progress_bar_styles: None,
    progress_value_styles: None,
    meter_bar_styles: None,
    meter_optimum_value_styles: None,
    meter_suboptimum_value_styles: None,
    meter_even_less_good_value_styles: None,
    assigned_slot: None,
    slotted_node_ids: Vec::new(),
    children: vec![],
  };

  let mut counters = CounterManager::new();
  counters.enter_scope();
  counters.apply_reset(&CounterSet::single("list-item", 1));

  let mut quote_depth = 0usize;
  let marker_box = create_marker_box(&styled, &mut counters, &mut quote_depth).expect("marker");
  counters.leave_scope();

  match &marker_box.box_type {
    BoxType::Marker(marker) => match &marker.content {
      MarkerContent::Text(t) => assert_eq!(t.as_str(), "▸ "),
      MarkerContent::Image(_) => panic!("expected text marker from disclosure-closed"),
    },
    _ => panic!("expected marker box"),
  }
}

#[test]
fn disclosure_closed_marker_points_left_in_rtl() {
  let mut style = ComputedStyle::default();
  style.display = Display::ListItem;
  style.list_style_type = ListStyleType::DisclosureClosed;
  style.direction = Direction::Rtl;
  style.writing_mode = WritingMode::HorizontalTb;
  let style = Arc::new(style);

  let styled = StyledNode {
    node_id: 0,
    subtree_size: 1,
    node: dom::DomNode {
      node_type: dom::DomNodeType::Element {
        tag_name: "li".to_string(),
        namespace: HTML_NAMESPACE.to_string(),
        attributes: vec![],
      },
      children: vec![],
    },
    styles: style.clone(),
    marker_styles: None,
    starting_styles: StartingStyleSet::default(),
    before_styles: None,
    after_styles: None,
    first_line_styles: None,
    first_letter_styles: None,
    placeholder_styles: None,
    file_selector_button_styles: None,
    footnote_call_styles: None,
    footnote_marker_styles: None,
    slider_thumb_styles: None,
    slider_track_styles: None,
    progress_bar_styles: None,
    progress_value_styles: None,
    meter_bar_styles: None,
    meter_optimum_value_styles: None,
    meter_suboptimum_value_styles: None,
    meter_even_less_good_value_styles: None,
    assigned_slot: None,
    slotted_node_ids: Vec::new(),
    children: vec![],
  };

  let mut counters = CounterManager::new();
  counters.enter_scope();
  counters.apply_reset(&CounterSet::single("list-item", 1));

  let mut quote_depth = 0usize;
  let marker_box = create_marker_box(&styled, &mut counters, &mut quote_depth).expect("marker");
  counters.leave_scope();

  match &marker_box.box_type {
    BoxType::Marker(marker) => match &marker.content {
      MarkerContent::Text(t) => assert_eq!(t.as_str(), "◂ "),
      MarkerContent::Image(_) => panic!("expected text marker from disclosure-closed"),
    },
    _ => panic!("expected marker box"),
  }
}

#[test]
fn disc_marker_has_space_suffix() {
  let mut style = ComputedStyle::default();
  style.display = Display::ListItem;
  style.list_style_type = ListStyleType::Disc;
  let style = Arc::new(style);
  let styled = StyledNode {
    node_id: 0,
    subtree_size: 1,
    node: dom::DomNode {
      node_type: dom::DomNodeType::Element {
        tag_name: "li".to_string(),
        namespace: HTML_NAMESPACE.to_string(),
        attributes: vec![],
      },
      children: vec![],
    },
    styles: style.clone(),
    marker_styles: None,
    starting_styles: StartingStyleSet::default(),
    before_styles: None,
    after_styles: None,
    first_line_styles: None,
    first_letter_styles: None,
    placeholder_styles: None,
    file_selector_button_styles: None,
    footnote_call_styles: None,
    footnote_marker_styles: None,
    slider_thumb_styles: None,
    slider_track_styles: None,
    progress_bar_styles: None,
    progress_value_styles: None,
    meter_bar_styles: None,
    meter_optimum_value_styles: None,
    meter_suboptimum_value_styles: None,
    meter_even_less_good_value_styles: None,
    assigned_slot: None,
    slotted_node_ids: Vec::new(),
    children: vec![],
  };

  let mut counters = CounterManager::new();
  counters.enter_scope();
  counters.apply_reset(&CounterSet::single("list-item", 1));

  let mut quote_depth = 0usize;
  let marker_box = create_marker_box(&styled, &mut counters, &mut quote_depth).expect("marker");
  counters.leave_scope();

  match &marker_box.box_type {
    BoxType::Marker(marker) => match &marker.content {
      MarkerContent::Text(t) => assert_eq!(t.as_str(), "• "),
      MarkerContent::Image(_) => panic!("expected text marker from disc"),
    },
    _ => panic!("expected marker box"),
  }
}

#[test]
fn marker_uses_custom_counter_style_definition() {
  let mut registry = CounterStyleRegistry::with_builtins();
  let mut rule = CounterStyleRule::new("custom-mark");
  rule.system = Some(CounterSystem::Cyclic);
  rule.symbols = Some(vec!["◇".into()]);
  registry.register(rule);
  let registry = Arc::new(registry);

  let mut style = ComputedStyle::default();
  style.counter_styles = registry.clone();
  style.list_style_type = ListStyleType::Custom("custom-mark".to_string());
  style.display = Display::ListItem;
  let style = Arc::new(style);

  let styled = StyledNode {
    node_id: 0,
    subtree_size: 1,
    node: dom::DomNode {
      node_type: dom::DomNodeType::Element {
        tag_name: "li".to_string(),
        namespace: HTML_NAMESPACE.to_string(),
        attributes: vec![],
      },
      children: vec![],
    },
    styles: style.clone(),
    marker_styles: Some(style.clone()),
    placeholder_styles: None,
    file_selector_button_styles: None,
    footnote_call_styles: None,
    footnote_marker_styles: None,
    starting_styles: StartingStyleSet::default(),
    before_styles: None,
    after_styles: None,
    first_line_styles: None,
    first_letter_styles: None,
    slider_thumb_styles: None,
    slider_track_styles: None,
    progress_bar_styles: None,
    progress_value_styles: None,
    meter_bar_styles: None,
    meter_optimum_value_styles: None,
    meter_suboptimum_value_styles: None,
    meter_even_less_good_value_styles: None,
    assigned_slot: None,
    slotted_node_ids: Vec::new(),
    children: vec![],
  };

  let mut counters = CounterManager::new_with_styles(registry);
  counters.enter_scope();
  counters.apply_reset(&CounterSet::single("list-item", 1));

  let mut quote_depth = 0usize;
  let marker_box = create_marker_box(&styled, &mut counters, &mut quote_depth).expect("marker");
  counters.leave_scope();

  match &marker_box.box_type {
    BoxType::Marker(marker) => match &marker.content {
      MarkerContent::Text(t) => assert_eq!(t.as_str(), "◇. "),
      MarkerContent::Image(_) => panic!("expected text marker"),
    },
    _ => panic!("expected marker box"),
  }
}

#[test]
fn decimal_marker_has_dot_suffix() {
  let mut style = ComputedStyle::default();
  style.list_style_type = ListStyleType::Decimal;
  style.display = Display::ListItem;
  let style = Arc::new(style);

  let styled = StyledNode {
    node_id: 0,
    subtree_size: 1,
    node: dom::DomNode {
      node_type: dom::DomNodeType::Element {
        tag_name: "li".to_string(),
        namespace: HTML_NAMESPACE.to_string(),
        attributes: vec![],
      },
      children: vec![],
    },
    styles: style.clone(),
    marker_styles: None,
    starting_styles: StartingStyleSet::default(),
    before_styles: None,
    after_styles: None,
    first_line_styles: None,
    first_letter_styles: None,
    placeholder_styles: None,
    file_selector_button_styles: None,
    footnote_call_styles: None,
    footnote_marker_styles: None,
    slider_thumb_styles: None,
    slider_track_styles: None,
    progress_bar_styles: None,
    progress_value_styles: None,
    meter_bar_styles: None,
    meter_optimum_value_styles: None,
    meter_suboptimum_value_styles: None,
    meter_even_less_good_value_styles: None,
    assigned_slot: None,
    slotted_node_ids: Vec::new(),
    children: vec![],
  };

  let mut counters = CounterManager::new();
  counters.enter_scope();
  counters.apply_reset(&CounterSet::single("list-item", 1));

  let mut quote_depth = 0usize;
  let marker_box = create_marker_box(&styled, &mut counters, &mut quote_depth).expect("marker");
  counters.leave_scope();

  match &marker_box.box_type {
    BoxType::Marker(marker) => match &marker.content {
      MarkerContent::Text(t) => assert_eq!(t.as_str(), "1. "),
      MarkerContent::Image(_) => panic!("expected text marker"),
    },
    _ => panic!("expected marker box"),
  }
}

#[test]
fn custom_counter_style_prefix_suffix_descriptors_are_used_for_marker() {
  let mut registry = CounterStyleRegistry::with_builtins();
  let mut rule = CounterStyleRule::new("custom-mark");
  rule.system = Some(CounterSystem::Cyclic);
  rule.symbols = Some(vec!["◇".into()]);
  rule.prefix = Some("(".to_string());
  rule.suffix = Some(") ".to_string());
  registry.register(rule);
  let registry = Arc::new(registry);

  let mut style = ComputedStyle::default();
  style.counter_styles = registry.clone();
  style.list_style_type = ListStyleType::Custom("custom-mark".to_string());
  style.display = Display::ListItem;
  let style = Arc::new(style);

  let styled = StyledNode {
    node_id: 0,
    subtree_size: 1,
    node: dom::DomNode {
      node_type: dom::DomNodeType::Element {
        tag_name: "li".to_string(),
        namespace: HTML_NAMESPACE.to_string(),
        attributes: vec![],
      },
      children: vec![],
    },
    styles: style.clone(),
    marker_styles: Some(style.clone()),
    starting_styles: StartingStyleSet::default(),
    before_styles: None,
    after_styles: None,
    first_line_styles: None,
    first_letter_styles: None,
    placeholder_styles: None,
    file_selector_button_styles: None,
    footnote_call_styles: None,
    footnote_marker_styles: None,
    slider_thumb_styles: None,
    slider_track_styles: None,
    progress_bar_styles: None,
    progress_value_styles: None,
    meter_bar_styles: None,
    meter_optimum_value_styles: None,
    meter_suboptimum_value_styles: None,
    meter_even_less_good_value_styles: None,
    assigned_slot: None,
    slotted_node_ids: Vec::new(),
    children: vec![],
  };

  let mut counters = CounterManager::new_with_styles(registry);
  counters.enter_scope();
  counters.apply_reset(&CounterSet::single("list-item", 1));

  let mut quote_depth = 0usize;
  let marker_box = create_marker_box(&styled, &mut counters, &mut quote_depth).expect("marker");
  counters.leave_scope();

  match &marker_box.box_type {
    BoxType::Marker(marker) => match &marker.content {
      MarkerContent::Text(t) => assert_eq!(t.as_str(), "(◇) "),
      MarkerContent::Image(_) => panic!("expected text marker"),
    },
    _ => panic!("expected marker box"),
  }
}

#[test]
fn custom_counter_style_extends_inherits_prefix_suffix_for_marker() {
  let mut registry = CounterStyleRegistry::with_builtins();
  let mut base = CounterStyleRule::new("base-mark");
  base.system = Some(CounterSystem::Cyclic);
  base.symbols = Some(vec!["◇".into()]);
  base.prefix = Some("(".to_string());
  base.suffix = Some(") ".to_string());
  registry.register(base);

  let mut derived = CounterStyleRule::new("derived-mark");
  derived.system = Some(CounterSystem::Extends("base-mark".to_string()));
  derived.symbols = Some(vec!["◆".into()]);
  registry.register(derived);
  let registry = Arc::new(registry);

  let mut style = ComputedStyle::default();
  style.counter_styles = registry.clone();
  style.list_style_type = ListStyleType::Custom("derived-mark".to_string());
  style.display = Display::ListItem;
  let style = Arc::new(style);

  let styled = StyledNode {
    node_id: 0,
    subtree_size: 1,
    node: dom::DomNode {
      node_type: dom::DomNodeType::Element {
        tag_name: "li".to_string(),
        namespace: HTML_NAMESPACE.to_string(),
        attributes: vec![],
      },
      children: vec![],
    },
    styles: style.clone(),
    marker_styles: Some(style.clone()),
    starting_styles: StartingStyleSet::default(),
    before_styles: None,
    after_styles: None,
    first_line_styles: None,
    first_letter_styles: None,
    placeholder_styles: None,
    file_selector_button_styles: None,
    footnote_call_styles: None,
    footnote_marker_styles: None,
    slider_thumb_styles: None,
    slider_track_styles: None,
    progress_bar_styles: None,
    progress_value_styles: None,
    meter_bar_styles: None,
    meter_optimum_value_styles: None,
    meter_suboptimum_value_styles: None,
    meter_even_less_good_value_styles: None,
    assigned_slot: None,
    slotted_node_ids: Vec::new(),
    children: vec![],
  };

  let mut counters = CounterManager::new_with_styles(registry);
  counters.enter_scope();
  counters.apply_reset(&CounterSet::single("list-item", 1));

  let mut quote_depth = 0usize;
  let marker_box = create_marker_box(&styled, &mut counters, &mut quote_depth).expect("marker");
  counters.leave_scope();

  match &marker_box.box_type {
    BoxType::Marker(marker) => match &marker.content {
      MarkerContent::Text(t) => assert_eq!(t.as_str(), "(◆) "),
      MarkerContent::Image(_) => panic!("expected text marker"),
    },
    _ => panic!("expected marker box"),
  }
}

#[test]
fn custom_counter_style_extends_cycle_falls_back_to_decimal_marker() {
  let mut registry = CounterStyleRegistry::with_builtins();
  let mut a = CounterStyleRule::new("cycle-a");
  a.system = Some(CounterSystem::Extends("cycle-b".to_string()));
  a.symbols = Some(vec!["A".into()]);
  registry.register(a);
  let mut b = CounterStyleRule::new("cycle-b");
  b.system = Some(CounterSystem::Extends("cycle-a".to_string()));
  b.symbols = Some(vec!["B".into()]);
  registry.register(b);
  let registry = Arc::new(registry);

  let mut style = ComputedStyle::default();
  style.counter_styles = registry.clone();
  style.list_style_type = ListStyleType::Custom("cycle-a".to_string());
  style.display = Display::ListItem;
  let style = Arc::new(style);

  let styled = StyledNode {
    node_id: 0,
    subtree_size: 1,
    node: dom::DomNode {
      node_type: dom::DomNodeType::Element {
        tag_name: "li".to_string(),
        namespace: HTML_NAMESPACE.to_string(),
        attributes: vec![],
      },
      children: vec![],
    },
    styles: style.clone(),
    marker_styles: Some(style.clone()),
    starting_styles: StartingStyleSet::default(),
    before_styles: None,
    after_styles: None,
    first_line_styles: None,
    first_letter_styles: None,
    placeholder_styles: None,
    file_selector_button_styles: None,
    footnote_call_styles: None,
    footnote_marker_styles: None,
    slider_thumb_styles: None,
    slider_track_styles: None,
    progress_bar_styles: None,
    progress_value_styles: None,
    meter_bar_styles: None,
    meter_optimum_value_styles: None,
    meter_suboptimum_value_styles: None,
    meter_even_less_good_value_styles: None,
    assigned_slot: None,
    slotted_node_ids: Vec::new(),
    children: vec![],
  };

  let mut counters = CounterManager::new_with_styles(registry);
  counters.enter_scope();
  counters.apply_reset(&CounterSet::single("list-item", 1));

  let mut quote_depth = 0usize;
  let marker_box = create_marker_box(&styled, &mut counters, &mut quote_depth).expect("marker");
  counters.leave_scope();

  match &marker_box.box_type {
    BoxType::Marker(marker) => match &marker.content {
      MarkerContent::Text(t) => assert_eq!(t.as_str(), "1. "),
      MarkerContent::Image(_) => panic!("expected text marker"),
    },
    _ => panic!("expected marker box"),
  }
}

#[test]
fn unordered_list_decimal_starts_at_one() {
  let mut ul_style = ComputedStyle::default();
  ul_style.display = Display::Block;
  let ul_style = Arc::new(ul_style);

  let mut li_style = ComputedStyle::default();
  li_style.display = Display::ListItem;
  li_style.list_style_type = ListStyleType::Decimal;
  let li_style = Arc::new(li_style);

  let mk_li = |node_id: usize| StyledNode {
    node_id,
    subtree_size: 1,
    node: dom::DomNode {
      node_type: dom::DomNodeType::Element {
        tag_name: "li".to_string(),
        namespace: HTML_NAMESPACE.to_string(),
        attributes: vec![],
      },
      children: vec![],
    },
    styles: li_style.clone(),
    starting_styles: StartingStyleSet::default(),
    before_styles: None,
    after_styles: None,
    marker_styles: None,
    first_line_styles: None,
    first_letter_styles: None,
    placeholder_styles: None,
    file_selector_button_styles: None,
    footnote_call_styles: None,
    footnote_marker_styles: None,
    slider_thumb_styles: None,
    slider_track_styles: None,
    progress_bar_styles: None,
    progress_value_styles: None,
    meter_bar_styles: None,
    meter_optimum_value_styles: None,
    meter_suboptimum_value_styles: None,
    meter_even_less_good_value_styles: None,
    assigned_slot: None,
    slotted_node_ids: Vec::new(),
    children: vec![],
  };

  let ul = StyledNode {
    node_id: 0,
    subtree_size: 1,
    node: dom::DomNode {
      node_type: dom::DomNodeType::Element {
        tag_name: "ul".to_string(),
        namespace: HTML_NAMESPACE.to_string(),
        attributes: vec![],
      },
      children: vec![],
    },
    styles: ul_style,
    starting_styles: StartingStyleSet::default(),
    before_styles: None,
    after_styles: None,
    marker_styles: None,
    first_line_styles: None,
    first_letter_styles: None,
    placeholder_styles: None,
    file_selector_button_styles: None,
    footnote_call_styles: None,
    footnote_marker_styles: None,
    slider_thumb_styles: None,
    slider_track_styles: None,
    progress_bar_styles: None,
    progress_value_styles: None,
    meter_bar_styles: None,
    meter_optimum_value_styles: None,
    meter_suboptimum_value_styles: None,
    meter_even_less_good_value_styles: None,
    assigned_slot: None,
    slotted_node_ids: Vec::new(),
    children: vec![mk_li(1), mk_li(2), mk_li(3)],
  };

  let tree = generate_box_tree(&ul);

  fn collect_marker_numbers(node: &BoxNode, out: &mut Vec<i32>) {
    if let BoxType::Marker(marker) = &node.box_type {
      if let MarkerContent::Text(text) = &marker.content {
        out.push(marker_leading_decimal(text));
      }
    }
    for child in node.children.iter() {
      collect_marker_numbers(child, out);
    }
  }

  let mut markers = Vec::new();
  collect_marker_numbers(&tree.root, &mut markers);
  assert_eq!(markers, vec![1, 2, 3]);
}

#[test]
fn ordered_list_start_attribute_sets_initial_counter() {
  let mut ol_style = ComputedStyle::default();
  ol_style.display = Display::Block;
  let ol_style = Arc::new(ol_style);

  let mut li_style = ComputedStyle::default();
  li_style.display = Display::ListItem;
  li_style.list_style_type = ListStyleType::Decimal;
  let li_style = Arc::new(li_style);

  let ol_dom = dom::DomNode {
    node_type: dom::DomNodeType::Element {
      tag_name: "ol".to_string(),
      namespace: HTML_NAMESPACE.to_string(),
      attributes: vec![("start".to_string(), "5".to_string())],
    },
    children: vec![],
  };

  let mk_li = |text: &str| StyledNode {
    node_id: 0,
    subtree_size: 1,
    node: dom::DomNode {
      node_type: dom::DomNodeType::Element {
        tag_name: "li".to_string(),
        namespace: HTML_NAMESPACE.to_string(),
        attributes: vec![],
      },
      children: vec![dom::DomNode {
        node_type: dom::DomNodeType::Text {
          content: text.to_string(),
        },
        children: vec![],
      }],
    },
    styles: li_style.clone(),
    starting_styles: StartingStyleSet::default(),
    before_styles: None,
    after_styles: None,
    marker_styles: None,
    placeholder_styles: None,
    file_selector_button_styles: None,
    footnote_call_styles: None,
    footnote_marker_styles: None,
    first_line_styles: None,
    first_letter_styles: None,
    slider_thumb_styles: None,
    slider_track_styles: None,
    progress_bar_styles: None,
    progress_value_styles: None,
    meter_bar_styles: None,
    meter_optimum_value_styles: None,
    meter_suboptimum_value_styles: None,
    meter_even_less_good_value_styles: None,
    assigned_slot: None,
    slotted_node_ids: Vec::new(),
    children: vec![StyledNode {
      node_id: 0,
      subtree_size: 1,
      node: dom::DomNode {
        node_type: dom::DomNodeType::Text {
          content: text.to_string(),
        },
        children: vec![],
      },
      styles: default_style(),
      starting_styles: StartingStyleSet::default(),
      before_styles: None,
      after_styles: None,
      marker_styles: None,
      placeholder_styles: None,
      file_selector_button_styles: None,
      footnote_call_styles: None,
      footnote_marker_styles: None,
      first_line_styles: None,
      first_letter_styles: None,
      slider_thumb_styles: None,
      slider_track_styles: None,
      progress_bar_styles: None,
      progress_value_styles: None,
      meter_bar_styles: None,
      meter_optimum_value_styles: None,
      meter_suboptimum_value_styles: None,
      meter_even_less_good_value_styles: None,
      assigned_slot: None,
      slotted_node_ids: Vec::new(),
      children: vec![],
    }],
  };

  let ol = StyledNode {
    node_id: 0,
    subtree_size: 1,
    node: ol_dom,
    styles: ol_style,
    starting_styles: StartingStyleSet::default(),
    before_styles: None,
    after_styles: None,
    marker_styles: None,
    placeholder_styles: None,
    file_selector_button_styles: None,
    footnote_call_styles: None,
    footnote_marker_styles: None,
    first_line_styles: None,
    first_letter_styles: None,
    slider_thumb_styles: None,
    slider_track_styles: None,
    progress_bar_styles: None,
    progress_value_styles: None,
    meter_bar_styles: None,
    meter_optimum_value_styles: None,
    meter_suboptimum_value_styles: None,
    meter_even_less_good_value_styles: None,
    assigned_slot: None,
    slotted_node_ids: Vec::new(),
    children: vec![mk_li("one"), mk_li("two"), mk_li("three")],
  };

  let tree = generate_box_tree(&ol);
  let markers: Vec<i32> = tree
    .root
    .children
    .iter()
    .filter_map(|li| {
      li.children.first().and_then(|child| match &child.box_type {
        BoxType::Marker(m) => match &m.content {
          MarkerContent::Text(t) => Some(marker_leading_decimal(t)),
          MarkerContent::Image(_) => None,
        },
        _ => None,
      })
    })
    .collect();

  assert_eq!(markers, vec![5, 6, 7]);
}

#[test]
fn ordered_list_defaults_start_at_one() {
  let mut ol_style = ComputedStyle::default();
  ol_style.display = Display::Block;
  let ol_style = Arc::new(ol_style);

  let mut li_style = ComputedStyle::default();
  li_style.display = Display::ListItem;
  li_style.list_style_type = ListStyleType::Decimal;
  let li_style = Arc::new(li_style);

  let ol_dom = dom::DomNode {
    node_type: dom::DomNodeType::Element {
      tag_name: "ol".to_string(),
      namespace: HTML_NAMESPACE.to_string(),
      attributes: vec![],
    },
    children: vec![],
  };

  let mk_li = |text: &str| StyledNode {
    node_id: 0,
    subtree_size: 1,
    node: dom::DomNode {
      node_type: dom::DomNodeType::Element {
        tag_name: "li".to_string(),
        namespace: HTML_NAMESPACE.to_string(),
        attributes: vec![],
      },
      children: vec![dom::DomNode {
        node_type: dom::DomNodeType::Text {
          content: text.to_string(),
        },
        children: vec![],
      }],
    },
    styles: li_style.clone(),
    starting_styles: StartingStyleSet::default(),
    before_styles: None,
    after_styles: None,
    marker_styles: None,
    placeholder_styles: None,
    file_selector_button_styles: None,
    footnote_call_styles: None,
    footnote_marker_styles: None,
    first_line_styles: None,
    first_letter_styles: None,
    slider_thumb_styles: None,
    slider_track_styles: None,
    progress_bar_styles: None,
    progress_value_styles: None,
    meter_bar_styles: None,
    meter_optimum_value_styles: None,
    meter_suboptimum_value_styles: None,
    meter_even_less_good_value_styles: None,
    assigned_slot: None,
    slotted_node_ids: Vec::new(),
    children: vec![StyledNode {
      node_id: 0,
      subtree_size: 1,
      node: dom::DomNode {
        node_type: dom::DomNodeType::Text {
          content: text.to_string(),
        },
        children: vec![],
      },
      styles: default_style(),
      starting_styles: StartingStyleSet::default(),
      before_styles: None,
      after_styles: None,
      marker_styles: None,
      placeholder_styles: None,
      file_selector_button_styles: None,
      footnote_call_styles: None,
      footnote_marker_styles: None,
      first_line_styles: None,
      first_letter_styles: None,
      slider_thumb_styles: None,
      slider_track_styles: None,
      progress_bar_styles: None,
      progress_value_styles: None,
      meter_bar_styles: None,
      meter_optimum_value_styles: None,
      meter_suboptimum_value_styles: None,
      meter_even_less_good_value_styles: None,
      assigned_slot: None,
      slotted_node_ids: Vec::new(),
      children: vec![],
    }],
  };

  let ol = StyledNode {
    node_id: 0,
    subtree_size: 1,
    node: ol_dom,
    styles: ol_style,
    starting_styles: StartingStyleSet::default(),
    before_styles: None,
    after_styles: None,
    marker_styles: None,
    placeholder_styles: None,
    file_selector_button_styles: None,
    footnote_call_styles: None,
    footnote_marker_styles: None,
    first_line_styles: None,
    first_letter_styles: None,
    slider_thumb_styles: None,
    slider_track_styles: None,
    progress_bar_styles: None,
    progress_value_styles: None,
    meter_bar_styles: None,
    meter_optimum_value_styles: None,
    meter_suboptimum_value_styles: None,
    meter_even_less_good_value_styles: None,
    assigned_slot: None,
    slotted_node_ids: Vec::new(),
    children: vec![mk_li("one"), mk_li("two"), mk_li("three")],
  };

  let tree = generate_box_tree(&ol);
  let markers: Vec<i32> = tree
    .root
    .children
    .iter()
    .filter_map(|li| {
      li.children.first().and_then(|child| match &child.box_type {
        BoxType::Marker(m) => match &m.content {
          MarkerContent::Text(t) => Some(marker_leading_decimal(t)),
          MarkerContent::Image(_) => None,
        },
        _ => None,
      })
    })
    .collect();

  assert_eq!(markers, vec![1, 2, 3]);
}

#[test]
fn list_item_implicit_increment_survives_counter_increment_none() {
  let mut ol_style = ComputedStyle::default();
  ol_style.display = Display::Block;
  let ol_style = Arc::new(ol_style);

  let mut li_style = ComputedStyle::default();
  li_style.display = Display::ListItem;
  li_style.list_style_type = ListStyleType::Decimal;
  li_style.counters.counter_increment = Some(CounterSet::new());
  let li_style = Arc::new(li_style);

  let text_node = |content: &str| StyledNode {
    node_id: 0,
    subtree_size: 1,
    node: dom::DomNode {
      node_type: dom::DomNodeType::Text {
        content: content.to_string(),
      },
      children: vec![],
    },
    styles: default_style(),
    starting_styles: StartingStyleSet::default(),
    before_styles: None,
    after_styles: None,
    marker_styles: None,
    first_line_styles: None,
    first_letter_styles: None,
    placeholder_styles: None,
    file_selector_button_styles: None,
    footnote_call_styles: None,
    footnote_marker_styles: None,
    slider_thumb_styles: None,
    slider_track_styles: None,
    progress_bar_styles: None,
    progress_value_styles: None,
    meter_bar_styles: None,
    meter_optimum_value_styles: None,
    meter_suboptimum_value_styles: None,
    meter_even_less_good_value_styles: None,
    assigned_slot: None,
    slotted_node_ids: Vec::new(),
    children: vec![],
  };

  let li = |content: &str| StyledNode {
    node_id: 0,
    subtree_size: 1,
    node: dom::DomNode {
      node_type: dom::DomNodeType::Element {
        tag_name: "li".to_string(),
        namespace: HTML_NAMESPACE.to_string(),
        attributes: vec![],
      },
      children: vec![],
    },
    styles: li_style.clone(),
    starting_styles: StartingStyleSet::default(),
    before_styles: None,
    after_styles: None,
    marker_styles: None,
    first_line_styles: None,
    first_letter_styles: None,
    placeholder_styles: None,
    file_selector_button_styles: None,
    footnote_call_styles: None,
    footnote_marker_styles: None,
    slider_thumb_styles: None,
    slider_track_styles: None,
    progress_bar_styles: None,
    progress_value_styles: None,
    meter_bar_styles: None,
    meter_optimum_value_styles: None,
    meter_suboptimum_value_styles: None,
    meter_even_less_good_value_styles: None,
    assigned_slot: None,
    slotted_node_ids: Vec::new(),
    children: vec![text_node(content)],
  };

  let ol = StyledNode {
    node_id: 0,
    subtree_size: 1,
    node: dom::DomNode {
      node_type: dom::DomNodeType::Element {
        tag_name: "ol".to_string(),
        namespace: HTML_NAMESPACE.to_string(),
        attributes: vec![],
      },
      children: vec![],
    },
    styles: ol_style,
    starting_styles: StartingStyleSet::default(),
    before_styles: None,
    after_styles: None,
    marker_styles: None,
    first_line_styles: None,
    first_letter_styles: None,
    placeholder_styles: None,
    file_selector_button_styles: None,
    footnote_call_styles: None,
    footnote_marker_styles: None,
    slider_thumb_styles: None,
    slider_track_styles: None,
    progress_bar_styles: None,
    progress_value_styles: None,
    meter_bar_styles: None,
    meter_optimum_value_styles: None,
    meter_suboptimum_value_styles: None,
    meter_even_less_good_value_styles: None,
    assigned_slot: None,
    slotted_node_ids: Vec::new(),
    children: vec![li("one"), li("two")],
  };

  let tree = generate_box_tree(&ol);
  let markers: Vec<i32> = tree
    .root
    .children
    .iter()
    .filter_map(|li| {
      li.children.first().and_then(|child| match &child.box_type {
        BoxType::Marker(m) => match &m.content {
          MarkerContent::Text(t) => Some(marker_leading_decimal(t)),
          MarkerContent::Image(_) => None,
        },
        _ => None,
      })
    })
    .collect();

  assert_eq!(markers, vec![1, 2]);
}

#[test]
fn list_item_implicit_increment_survives_counter_increment_other_counter() {
  let mut ol_style = ComputedStyle::default();
  ol_style.display = Display::Block;
  let ol_style = Arc::new(ol_style);

  let mut li_style = ComputedStyle::default();
  li_style.display = Display::ListItem;
  li_style.list_style_type = ListStyleType::Decimal;
  li_style.counters.counter_increment = Some(CounterSet::single("foo", 1));
  let li_style = Arc::new(li_style);

  let mut before_style = ComputedStyle::default();
  before_style.content_value = ContentValue::Items(vec![ContentItem::Counter {
    name: "foo".to_string(),
    style: None,
  }]);
  let before_style = Arc::new(before_style);

  let text_node = |content: &str| StyledNode {
    node_id: 0,
    subtree_size: 1,
    node: dom::DomNode {
      node_type: dom::DomNodeType::Text {
        content: content.to_string(),
      },
      children: vec![],
    },
    styles: default_style(),
    starting_styles: StartingStyleSet::default(),
    before_styles: None,
    after_styles: None,
    marker_styles: None,
    first_line_styles: None,
    first_letter_styles: None,
    placeholder_styles: None,
    file_selector_button_styles: None,
    footnote_call_styles: None,
    footnote_marker_styles: None,
    slider_thumb_styles: None,
    slider_track_styles: None,
    progress_bar_styles: None,
    progress_value_styles: None,
    meter_bar_styles: None,
    meter_optimum_value_styles: None,
    meter_suboptimum_value_styles: None,
    meter_even_less_good_value_styles: None,
    assigned_slot: None,
    slotted_node_ids: Vec::new(),
    children: vec![],
  };

  let li = |content: &str| StyledNode {
    node_id: 0,
    subtree_size: 1,
    node: dom::DomNode {
      node_type: dom::DomNodeType::Element {
        tag_name: "li".to_string(),
        namespace: HTML_NAMESPACE.to_string(),
        attributes: vec![],
      },
      children: vec![],
    },
    styles: li_style.clone(),
    starting_styles: StartingStyleSet::default(),
    before_styles: Some(before_style.clone()),
    after_styles: None,
    marker_styles: None,
    first_line_styles: None,
    first_letter_styles: None,
    placeholder_styles: None,
    file_selector_button_styles: None,
    footnote_call_styles: None,
    footnote_marker_styles: None,
    slider_thumb_styles: None,
    slider_track_styles: None,
    progress_bar_styles: None,
    progress_value_styles: None,
    meter_bar_styles: None,
    meter_optimum_value_styles: None,
    meter_suboptimum_value_styles: None,
    meter_even_less_good_value_styles: None,
    assigned_slot: None,
    slotted_node_ids: Vec::new(),
    children: vec![text_node(content)],
  };

  let ol = StyledNode {
    node_id: 0,
    subtree_size: 1,
    node: dom::DomNode {
      node_type: dom::DomNodeType::Element {
        tag_name: "ol".to_string(),
        namespace: HTML_NAMESPACE.to_string(),
        attributes: vec![],
      },
      children: vec![],
    },
    styles: ol_style,
    starting_styles: StartingStyleSet::default(),
    before_styles: None,
    after_styles: None,
    marker_styles: None,
    first_line_styles: None,
    first_letter_styles: None,
    placeholder_styles: None,
    file_selector_button_styles: None,
    footnote_call_styles: None,
    footnote_marker_styles: None,
    slider_thumb_styles: None,
    slider_track_styles: None,
    progress_bar_styles: None,
    progress_value_styles: None,
    meter_bar_styles: None,
    meter_optimum_value_styles: None,
    meter_suboptimum_value_styles: None,
    meter_even_less_good_value_styles: None,
    assigned_slot: None,
    slotted_node_ids: Vec::new(),
    children: vec![li("one"), li("two")],
  };

  let tree = generate_box_tree(&ol);
  let markers: Vec<i32> = tree
    .root
    .children
    .iter()
    .filter_map(|li| {
      li.children.first().and_then(|child| match &child.box_type {
        BoxType::Marker(m) => match &m.content {
          MarkerContent::Text(t) => Some(marker_leading_decimal(t)),
          MarkerContent::Image(_) => None,
        },
        _ => None,
      })
    })
    .collect();

  assert_eq!(markers, vec![1, 2]);

  let before_values: Vec<i32> = tree
    .root
    .children
    .iter()
    .map(|li| {
      let before = li
        .children
        .iter()
        .find(|child| child.generated_pseudo == Some(GeneratedPseudoElement::Before))
        .expect("expected ::before pseudo-element box");
      let text = before
        .children
        .iter()
        .find_map(|child| child.text())
        .expect("expected ::before to contain text");
      text.parse::<i32>().expect("parse foo counter text")
    })
    .collect();

  assert_eq!(before_values, vec![1, 1]);
}

#[test]
fn display_none_list_items_do_not_increment_list_item_counter() {
  let mut ol_style = ComputedStyle::default();
  ol_style.display = Display::Block;
  let ol_style = Arc::new(ol_style);

  let mut li_style = ComputedStyle::default();
  li_style.display = Display::ListItem;
  li_style.list_style_type = ListStyleType::Decimal;
  let li_style = Arc::new(li_style);

  let mut li_hidden_style = ComputedStyle::default();
  li_hidden_style.display = Display::None;
  li_hidden_style.list_style_type = ListStyleType::Decimal;
  let li_hidden_style = Arc::new(li_hidden_style);

  let text_node = |content: &str| StyledNode {
    node_id: 0,
    subtree_size: 1,
    node: dom::DomNode {
      node_type: dom::DomNodeType::Text {
        content: content.to_string(),
      },
      children: vec![],
    },
    styles: default_style(),
    starting_styles: StartingStyleSet::default(),
    before_styles: None,
    after_styles: None,
    marker_styles: None,
    first_line_styles: None,
    first_letter_styles: None,
    placeholder_styles: None,
    file_selector_button_styles: None,
    footnote_call_styles: None,
    footnote_marker_styles: None,
    slider_thumb_styles: None,
    slider_track_styles: None,
    progress_bar_styles: None,
    progress_value_styles: None,
    meter_bar_styles: None,
    meter_optimum_value_styles: None,
    meter_suboptimum_value_styles: None,
    meter_even_less_good_value_styles: None,
    assigned_slot: None,
    slotted_node_ids: Vec::new(),
    children: vec![],
  };

  let li = |content: &str, hidden: bool| StyledNode {
    node_id: 0,
    subtree_size: 1,
    node: dom::DomNode {
      node_type: dom::DomNodeType::Element {
        tag_name: "li".to_string(),
        namespace: HTML_NAMESPACE.to_string(),
        attributes: vec![],
      },
      children: vec![],
    },
    styles: if hidden {
      li_hidden_style.clone()
    } else {
      li_style.clone()
    },
    starting_styles: StartingStyleSet::default(),
    before_styles: None,
    after_styles: None,
    marker_styles: None,
    first_line_styles: None,
    first_letter_styles: None,
    placeholder_styles: None,
    file_selector_button_styles: None,
    footnote_call_styles: None,
    footnote_marker_styles: None,
    slider_thumb_styles: None,
    slider_track_styles: None,
    progress_bar_styles: None,
    progress_value_styles: None,
    meter_bar_styles: None,
    meter_optimum_value_styles: None,
    meter_suboptimum_value_styles: None,
    meter_even_less_good_value_styles: None,
    assigned_slot: None,
    slotted_node_ids: Vec::new(),
    children: vec![text_node(content)],
  };

  let ol = StyledNode {
    node_id: 0,
    subtree_size: 1,
    node: dom::DomNode {
      node_type: dom::DomNodeType::Element {
        tag_name: "ol".to_string(),
        namespace: HTML_NAMESPACE.to_string(),
        attributes: vec![],
      },
      children: vec![],
    },
    styles: ol_style,
    starting_styles: StartingStyleSet::default(),
    before_styles: None,
    after_styles: None,
    marker_styles: None,
    first_line_styles: None,
    first_letter_styles: None,
    placeholder_styles: None,
    file_selector_button_styles: None,
    footnote_call_styles: None,
    footnote_marker_styles: None,
    slider_thumb_styles: None,
    slider_track_styles: None,
    progress_bar_styles: None,
    progress_value_styles: None,
    meter_bar_styles: None,
    meter_optimum_value_styles: None,
    meter_suboptimum_value_styles: None,
    meter_even_less_good_value_styles: None,
    assigned_slot: None,
    slotted_node_ids: Vec::new(),
    children: vec![li("one", false), li("two", true), li("three", false)],
  };

  let tree = generate_box_tree(&ol);
  assert_eq!(
    tree.root.children.len(),
    2,
    "hidden list items should not generate boxes"
  );

  let markers: Vec<i32> = tree
    .root
    .children
    .iter()
    .filter_map(|li| {
      li.children.first().and_then(|child| match &child.box_type {
        BoxType::Marker(m) => match &m.content {
          MarkerContent::Text(t) => Some(marker_leading_decimal(t)),
          MarkerContent::Image(_) => None,
        },
        _ => None,
      })
    })
    .collect();

  assert_eq!(markers, vec![1, 2]);
}

#[test]
fn reversed_ordered_list_counts_down() {
  let mut ol_style = ComputedStyle::default();
  ol_style.display = Display::Block;
  let ol_style = Arc::new(ol_style);

  let mut li_style = ComputedStyle::default();
  li_style.display = Display::ListItem;
  li_style.list_style_type = ListStyleType::Decimal;
  let li_style = Arc::new(li_style);

  let ol_dom = dom::DomNode {
    node_type: dom::DomNodeType::Element {
      tag_name: "ol".to_string(),
      namespace: HTML_NAMESPACE.to_string(),
      attributes: vec![("reversed".to_string(), String::new())],
    },
    children: vec![],
  };

  let mk_li = |text: &str| StyledNode {
    node_id: 0,
    subtree_size: 1,
    node: dom::DomNode {
      node_type: dom::DomNodeType::Element {
        tag_name: "li".to_string(),
        namespace: HTML_NAMESPACE.to_string(),
        attributes: vec![],
      },
      children: vec![dom::DomNode {
        node_type: dom::DomNodeType::Text {
          content: text.to_string(),
        },
        children: vec![],
      }],
    },
    styles: li_style.clone(),
    starting_styles: StartingStyleSet::default(),
    before_styles: None,
    after_styles: None,
    marker_styles: None,
    placeholder_styles: None,
    file_selector_button_styles: None,
    footnote_call_styles: None,
    footnote_marker_styles: None,
    first_line_styles: None,
    first_letter_styles: None,
    slider_thumb_styles: None,
    slider_track_styles: None,
    progress_bar_styles: None,
    progress_value_styles: None,
    meter_bar_styles: None,
    meter_optimum_value_styles: None,
    meter_suboptimum_value_styles: None,
    meter_even_less_good_value_styles: None,
    assigned_slot: None,
    slotted_node_ids: Vec::new(),
    children: vec![StyledNode {
      node_id: 0,
      subtree_size: 1,
      node: dom::DomNode {
        node_type: dom::DomNodeType::Text {
          content: text.to_string(),
        },
        children: vec![],
      },
      styles: default_style(),
      starting_styles: StartingStyleSet::default(),
      before_styles: None,
      after_styles: None,
      marker_styles: None,
      placeholder_styles: None,
      file_selector_button_styles: None,
      footnote_call_styles: None,
      footnote_marker_styles: None,
      first_line_styles: None,
      first_letter_styles: None,
      slider_thumb_styles: None,
      slider_track_styles: None,
      progress_bar_styles: None,
      progress_value_styles: None,
      meter_bar_styles: None,
      meter_optimum_value_styles: None,
      meter_suboptimum_value_styles: None,
      meter_even_less_good_value_styles: None,
      assigned_slot: None,
      slotted_node_ids: Vec::new(),
      children: vec![],
    }],
  };

  let ol = StyledNode {
    node_id: 0,
    subtree_size: 1,
    node: ol_dom,
    styles: ol_style,
    starting_styles: StartingStyleSet::default(),
    before_styles: None,
    after_styles: None,
    marker_styles: None,
    placeholder_styles: None,
    file_selector_button_styles: None,
    footnote_call_styles: None,
    footnote_marker_styles: None,
    first_line_styles: None,
    first_letter_styles: None,
    slider_thumb_styles: None,
    slider_track_styles: None,
    progress_bar_styles: None,
    progress_value_styles: None,
    meter_bar_styles: None,
    meter_optimum_value_styles: None,
    meter_suboptimum_value_styles: None,
    meter_even_less_good_value_styles: None,
    assigned_slot: None,
    slotted_node_ids: Vec::new(),
    children: vec![mk_li("one"), mk_li("two"), mk_li("three")],
  };

  let tree = generate_box_tree(&ol);
  let markers: Vec<i32> = tree
    .root
    .children
    .iter()
    .filter_map(|li| {
      li.children.first().and_then(|child| match &child.box_type {
        BoxType::Marker(m) => match &m.content {
          MarkerContent::Text(t) => Some(marker_leading_decimal(t)),
          MarkerContent::Image(_) => None,
        },
        _ => None,
      })
    })
    .collect();

  assert_eq!(markers, vec![3, 2, 1]);
}

#[test]
fn reversed_list_default_start_ignores_display_none_items() {
  let mut ol_style = ComputedStyle::default();
  ol_style.display = Display::Block;
  let ol_style = Arc::new(ol_style);

  let mut li_style = ComputedStyle::default();
  li_style.display = Display::ListItem;
  li_style.list_style_type = ListStyleType::Decimal;
  let li_style = Arc::new(li_style);

  let mut li_hidden_style = ComputedStyle::default();
  li_hidden_style.display = Display::None;
  li_hidden_style.list_style_type = ListStyleType::Decimal;
  let li_hidden_style = Arc::new(li_hidden_style);

  let text_node = |content: &str| StyledNode {
    node_id: 0,
    subtree_size: 1,
    node: dom::DomNode {
      node_type: dom::DomNodeType::Text {
        content: content.to_string(),
      },
      children: vec![],
    },
    styles: default_style(),
    starting_styles: StartingStyleSet::default(),
    before_styles: None,
    after_styles: None,
    marker_styles: None,
    first_line_styles: None,
    first_letter_styles: None,
    placeholder_styles: None,
    file_selector_button_styles: None,
    footnote_call_styles: None,
    footnote_marker_styles: None,
    slider_thumb_styles: None,
    slider_track_styles: None,
    progress_bar_styles: None,
    progress_value_styles: None,
    meter_bar_styles: None,
    meter_optimum_value_styles: None,
    meter_suboptimum_value_styles: None,
    meter_even_less_good_value_styles: None,
    assigned_slot: None,
    slotted_node_ids: Vec::new(),
    children: vec![],
  };

  let li = |content: &str, hidden: bool| StyledNode {
    node_id: 0,
    subtree_size: 1,
    node: dom::DomNode {
      node_type: dom::DomNodeType::Element {
        tag_name: "li".to_string(),
        namespace: HTML_NAMESPACE.to_string(),
        attributes: vec![],
      },
      children: vec![],
    },
    styles: if hidden {
      li_hidden_style.clone()
    } else {
      li_style.clone()
    },
    starting_styles: StartingStyleSet::default(),
    before_styles: None,
    after_styles: None,
    marker_styles: None,
    first_line_styles: None,
    first_letter_styles: None,
    placeholder_styles: None,
    file_selector_button_styles: None,
    footnote_call_styles: None,
    footnote_marker_styles: None,
    slider_thumb_styles: None,
    slider_track_styles: None,
    progress_bar_styles: None,
    progress_value_styles: None,
    meter_bar_styles: None,
    meter_optimum_value_styles: None,
    meter_suboptimum_value_styles: None,
    meter_even_less_good_value_styles: None,
    assigned_slot: None,
    slotted_node_ids: Vec::new(),
    children: vec![text_node(content)],
  };

  let ol = StyledNode {
    node_id: 0,
    subtree_size: 1,
    node: dom::DomNode {
      node_type: dom::DomNodeType::Element {
        tag_name: "ol".to_string(),
        namespace: HTML_NAMESPACE.to_string(),
        attributes: vec![("reversed".to_string(), String::new())],
      },
      children: vec![],
    },
    styles: ol_style,
    starting_styles: StartingStyleSet::default(),
    before_styles: None,
    after_styles: None,
    marker_styles: None,
    first_line_styles: None,
    first_letter_styles: None,
    placeholder_styles: None,
    file_selector_button_styles: None,
    footnote_call_styles: None,
    footnote_marker_styles: None,
    slider_thumb_styles: None,
    slider_track_styles: None,
    progress_bar_styles: None,
    progress_value_styles: None,
    meter_bar_styles: None,
    meter_optimum_value_styles: None,
    meter_suboptimum_value_styles: None,
    meter_even_less_good_value_styles: None,
    assigned_slot: None,
    slotted_node_ids: Vec::new(),
    children: vec![li("one", false), li("two", true), li("three", false)],
  };

  let tree = generate_box_tree(&ol);
  assert_eq!(
    tree.root.children.len(),
    2,
    "hidden list items should not generate boxes"
  );

  let markers: Vec<i32> = tree
    .root
    .children
    .iter()
    .filter_map(|li| {
      li.children.first().and_then(|child| match &child.box_type {
        BoxType::Marker(m) => match &m.content {
          MarkerContent::Text(t) => Some(marker_leading_decimal(t)),
          MarkerContent::Image(_) => None,
        },
        _ => None,
      })
    })
    .collect();

  assert_eq!(markers, vec![2, 1]);
}

#[test]
fn reversed_list_default_start_counts_items_inside_display_contents_lists() {
  use crate::dom::DomNodeType;
  use crate::style::types::ListStyleType;

  let mut ol_style = ComputedStyle::default();
  ol_style.display = Display::Block;
  let ol_style = Arc::new(ol_style);

  let mut li_style = ComputedStyle::default();
  li_style.display = Display::ListItem;
  li_style.list_style_type = ListStyleType::Decimal;
  let li_style = Arc::new(li_style);

  let mut contents_ol_style = ComputedStyle::default();
  contents_ol_style.display = Display::Contents;
  let contents_ol_style = Arc::new(contents_ol_style);

  let nested_li = StyledNode {
    node_id: 4,
    subtree_size: 1,
    node: dom::DomNode {
      node_type: DomNodeType::Element {
        tag_name: "li".to_string(),
        namespace: HTML_NAMESPACE.to_string(),
        attributes: vec![],
      },
      children: vec![],
    },
    styles: li_style.clone(),
    starting_styles: StartingStyleSet::default(),
    before_styles: None,
    after_styles: None,
    marker_styles: None,
    first_line_styles: None,
    first_letter_styles: None,
    placeholder_styles: None,
    file_selector_button_styles: None,
    footnote_call_styles: None,
    footnote_marker_styles: None,
    slider_thumb_styles: None,
    slider_track_styles: None,
    progress_bar_styles: None,
    progress_value_styles: None,
    meter_bar_styles: None,
    meter_optimum_value_styles: None,
    meter_suboptimum_value_styles: None,
    meter_even_less_good_value_styles: None,
    assigned_slot: None,
    slotted_node_ids: Vec::new(),
    children: vec![],
  };

  let nested_ol = StyledNode {
    node_id: 3,
    subtree_size: 1,
    node: dom::DomNode {
      node_type: DomNodeType::Element {
        tag_name: "ol".to_string(),
        namespace: HTML_NAMESPACE.to_string(),
        attributes: vec![],
      },
      children: vec![],
    },
    styles: contents_ol_style,
    starting_styles: StartingStyleSet::default(),
    before_styles: None,
    after_styles: None,
    marker_styles: None,
    first_line_styles: None,
    first_letter_styles: None,
    placeholder_styles: None,
    file_selector_button_styles: None,
    footnote_call_styles: None,
    footnote_marker_styles: None,
    slider_thumb_styles: None,
    slider_track_styles: None,
    progress_bar_styles: None,
    progress_value_styles: None,
    meter_bar_styles: None,
    meter_optimum_value_styles: None,
    meter_suboptimum_value_styles: None,
    meter_even_less_good_value_styles: None,
    assigned_slot: None,
    slotted_node_ids: Vec::new(),
    children: vec![nested_li],
  };

  let li1 = StyledNode {
    node_id: 1,
    subtree_size: 1,
    node: dom::DomNode {
      node_type: DomNodeType::Element {
        tag_name: "li".to_string(),
        namespace: HTML_NAMESPACE.to_string(),
        attributes: vec![],
      },
      children: vec![],
    },
    styles: li_style.clone(),
    starting_styles: StartingStyleSet::default(),
    before_styles: None,
    after_styles: None,
    marker_styles: None,
    first_line_styles: None,
    first_letter_styles: None,
    placeholder_styles: None,
    file_selector_button_styles: None,
    footnote_call_styles: None,
    footnote_marker_styles: None,
    slider_thumb_styles: None,
    slider_track_styles: None,
    progress_bar_styles: None,
    progress_value_styles: None,
    meter_bar_styles: None,
    meter_optimum_value_styles: None,
    meter_suboptimum_value_styles: None,
    meter_even_less_good_value_styles: None,
    assigned_slot: None,
    slotted_node_ids: Vec::new(),
    children: vec![],
  };

  let li2 = StyledNode {
    node_id: 2,
    subtree_size: 1,
    node: dom::DomNode {
      node_type: DomNodeType::Element {
        tag_name: "li".to_string(),
        namespace: HTML_NAMESPACE.to_string(),
        attributes: vec![],
      },
      children: vec![],
    },
    styles: li_style,
    starting_styles: StartingStyleSet::default(),
    before_styles: None,
    after_styles: None,
    marker_styles: None,
    first_line_styles: None,
    first_letter_styles: None,
    placeholder_styles: None,
    file_selector_button_styles: None,
    footnote_call_styles: None,
    footnote_marker_styles: None,
    slider_thumb_styles: None,
    slider_track_styles: None,
    progress_bar_styles: None,
    progress_value_styles: None,
    meter_bar_styles: None,
    meter_optimum_value_styles: None,
    meter_suboptimum_value_styles: None,
    meter_even_less_good_value_styles: None,
    assigned_slot: None,
    slotted_node_ids: Vec::new(),
    children: vec![nested_ol],
  };

  let ol = StyledNode {
    node_id: 0,
    subtree_size: 1,
    node: dom::DomNode {
      node_type: DomNodeType::Element {
        tag_name: "ol".to_string(),
        namespace: HTML_NAMESPACE.to_string(),
        attributes: vec![("reversed".to_string(), String::new())],
      },
      children: vec![],
    },
    styles: ol_style,
    starting_styles: StartingStyleSet::default(),
    before_styles: None,
    after_styles: None,
    marker_styles: None,
    first_line_styles: None,
    first_letter_styles: None,
    placeholder_styles: None,
    file_selector_button_styles: None,
    footnote_call_styles: None,
    footnote_marker_styles: None,
    slider_thumb_styles: None,
    slider_track_styles: None,
    progress_bar_styles: None,
    progress_value_styles: None,
    meter_bar_styles: None,
    meter_optimum_value_styles: None,
    meter_suboptimum_value_styles: None,
    meter_even_less_good_value_styles: None,
    assigned_slot: None,
    slotted_node_ids: Vec::new(),
    children: vec![li1, li2],
  };

  let tree = generate_box_tree(&ol);

  fn collect_marker_numbers(node: &BoxNode, out: &mut Vec<i32>) {
    if let BoxType::Marker(marker) = &node.box_type {
      if let MarkerContent::Text(text) = &marker.content {
        out.push(marker_leading_decimal(text));
      }
    }
    for child in node.children.iter() {
      collect_marker_numbers(child, out);
    }
  }

  let mut markers = Vec::new();
  collect_marker_numbers(&tree.root, &mut markers);
  assert_eq!(markers, vec![3, 2, 1]);
}

#[test]
fn li_value_attribute_sets_counter_for_that_item() {
  let mut ol_style = ComputedStyle::default();
  ol_style.display = Display::Block;
  let ol_style = Arc::new(ol_style);

  let mut li_style = ComputedStyle::default();
  li_style.display = Display::ListItem;
  li_style.list_style_type = ListStyleType::Decimal;
  let li_style = Arc::new(li_style);

  let ol_dom = dom::DomNode {
    node_type: dom::DomNodeType::Element {
      tag_name: "ol".to_string(),
      namespace: HTML_NAMESPACE.to_string(),
      attributes: vec![],
    },
    children: vec![],
  };

  let mk_li = |text: &str, value: Option<&str>| StyledNode {
    node_id: 0,
    subtree_size: 1,
    node: dom::DomNode {
      node_type: dom::DomNodeType::Element {
        tag_name: "li".to_string(),
        namespace: HTML_NAMESPACE.to_string(),
        attributes: value
          .map(|v| vec![("value".to_string(), v.to_string())])
          .unwrap_or_else(Vec::new),
      },
      children: vec![dom::DomNode {
        node_type: dom::DomNodeType::Text {
          content: text.to_string(),
        },
        children: vec![],
      }],
    },
    styles: li_style.clone(),
    starting_styles: StartingStyleSet::default(),
    before_styles: None,
    after_styles: None,
    marker_styles: None,
    placeholder_styles: None,
    file_selector_button_styles: None,
    footnote_call_styles: None,
    footnote_marker_styles: None,
    first_line_styles: None,
    first_letter_styles: None,
    slider_thumb_styles: None,
    slider_track_styles: None,
    progress_bar_styles: None,
    progress_value_styles: None,
    meter_bar_styles: None,
    meter_optimum_value_styles: None,
    meter_suboptimum_value_styles: None,
    meter_even_less_good_value_styles: None,
    assigned_slot: None,
    slotted_node_ids: Vec::new(),
    children: vec![StyledNode {
      node_id: 0,
      subtree_size: 1,
      node: dom::DomNode {
        node_type: dom::DomNodeType::Text {
          content: text.to_string(),
        },
        children: vec![],
      },
      styles: default_style(),
      starting_styles: StartingStyleSet::default(),
      before_styles: None,
      after_styles: None,
      marker_styles: None,
      placeholder_styles: None,
      file_selector_button_styles: None,
      footnote_call_styles: None,
      footnote_marker_styles: None,
      first_line_styles: None,
      first_letter_styles: None,
      slider_thumb_styles: None,
      slider_track_styles: None,
      progress_bar_styles: None,
      progress_value_styles: None,
      meter_bar_styles: None,
      meter_optimum_value_styles: None,
      meter_suboptimum_value_styles: None,
      meter_even_less_good_value_styles: None,
      assigned_slot: None,
      slotted_node_ids: Vec::new(),
      children: vec![],
    }],
  };

  let ol = StyledNode {
    node_id: 0,
    subtree_size: 1,
    node: ol_dom,
    styles: ol_style,
    starting_styles: StartingStyleSet::default(),
    before_styles: None,
    after_styles: None,
    marker_styles: None,
    placeholder_styles: None,
    file_selector_button_styles: None,
    footnote_call_styles: None,
    footnote_marker_styles: None,
    first_line_styles: None,
    first_letter_styles: None,
    slider_thumb_styles: None,
    slider_track_styles: None,
    progress_bar_styles: None,
    progress_value_styles: None,
    meter_bar_styles: None,
    meter_optimum_value_styles: None,
    meter_suboptimum_value_styles: None,
    meter_even_less_good_value_styles: None,
    assigned_slot: None,
    slotted_node_ids: Vec::new(),
    children: vec![
      mk_li("one", None),
      mk_li("two", Some("10")),
      mk_li("three", None),
    ],
  };

  let tree = generate_box_tree(&ol);
  let markers: Vec<i32> = tree
    .root
    .children
    .iter()
    .filter_map(|li| {
      li.children.first().and_then(|child| match &child.box_type {
        BoxType::Marker(m) => match &m.content {
          MarkerContent::Text(t) => Some(marker_leading_decimal(t)),
          MarkerContent::Image(_) => None,
        },
        _ => None,
      })
    })
    .collect();

  assert_eq!(markers, vec![1, 10, 11]);
}

#[test]
fn reversed_list_value_attribute_counts_down_from_value() {
  let mut ol_style = ComputedStyle::default();
  ol_style.display = Display::Block;
  let ol_style = Arc::new(ol_style);

  let mut li_style = ComputedStyle::default();
  li_style.display = Display::ListItem;
  li_style.list_style_type = ListStyleType::Decimal;
  let li_style = Arc::new(li_style);

  let ol_dom = dom::DomNode {
    node_type: dom::DomNodeType::Element {
      tag_name: "ol".to_string(),
      namespace: HTML_NAMESPACE.to_string(),
      attributes: vec![("reversed".to_string(), String::new())],
    },
    children: vec![],
  };

  let mk_li = |text: &str, value: Option<&str>| StyledNode {
    node_id: 0,
    subtree_size: 1,
    node: dom::DomNode {
      node_type: dom::DomNodeType::Element {
        tag_name: "li".to_string(),
        namespace: HTML_NAMESPACE.to_string(),
        attributes: value
          .map(|v| vec![("value".to_string(), v.to_string())])
          .unwrap_or_else(Vec::new),
      },
      children: vec![dom::DomNode {
        node_type: dom::DomNodeType::Text {
          content: text.to_string(),
        },
        children: vec![],
      }],
    },
    styles: li_style.clone(),
    starting_styles: StartingStyleSet::default(),
    before_styles: None,
    after_styles: None,
    marker_styles: None,
    placeholder_styles: None,
    file_selector_button_styles: None,
    footnote_call_styles: None,
    footnote_marker_styles: None,
    first_line_styles: None,
    first_letter_styles: None,
    slider_thumb_styles: None,
    slider_track_styles: None,
    progress_bar_styles: None,
    progress_value_styles: None,
    meter_bar_styles: None,
    meter_optimum_value_styles: None,
    meter_suboptimum_value_styles: None,
    meter_even_less_good_value_styles: None,
    assigned_slot: None,
    slotted_node_ids: Vec::new(),
    children: vec![StyledNode {
      node_id: 0,
      subtree_size: 1,
      node: dom::DomNode {
        node_type: dom::DomNodeType::Text {
          content: text.to_string(),
        },
        children: vec![],
      },
      styles: default_style(),
      starting_styles: StartingStyleSet::default(),
      before_styles: None,
      after_styles: None,
      marker_styles: None,
      placeholder_styles: None,
      file_selector_button_styles: None,
      footnote_call_styles: None,
      footnote_marker_styles: None,
      first_line_styles: None,
      first_letter_styles: None,
      slider_thumb_styles: None,
      slider_track_styles: None,
      progress_bar_styles: None,
      progress_value_styles: None,
      meter_bar_styles: None,
      meter_optimum_value_styles: None,
      meter_suboptimum_value_styles: None,
      meter_even_less_good_value_styles: None,
      assigned_slot: None,
      slotted_node_ids: Vec::new(),
      children: vec![],
    }],
  };

  let ol = StyledNode {
    node_id: 0,
    subtree_size: 1,
    node: ol_dom,
    styles: ol_style,
    starting_styles: StartingStyleSet::default(),
    before_styles: None,
    after_styles: None,
    marker_styles: None,
    placeholder_styles: None,
    file_selector_button_styles: None,
    footnote_call_styles: None,
    footnote_marker_styles: None,
    first_line_styles: None,
    first_letter_styles: None,
    slider_thumb_styles: None,
    slider_track_styles: None,
    progress_bar_styles: None,
    progress_value_styles: None,
    meter_bar_styles: None,
    meter_optimum_value_styles: None,
    meter_suboptimum_value_styles: None,
    meter_even_less_good_value_styles: None,
    assigned_slot: None,
    slotted_node_ids: Vec::new(),
    children: vec![
      mk_li("one", None),
      mk_li("two", Some("10")),
      mk_li("three", None),
    ],
  };

  let tree = generate_box_tree(&ol);
  let markers: Vec<i32> = tree
    .root
    .children
    .iter()
    .filter_map(|li| {
      li.children.first().and_then(|child| match &child.box_type {
        BoxType::Marker(m) => match &m.content {
          MarkerContent::Text(t) => Some(marker_leading_decimal(t)),
          MarkerContent::Image(_) => None,
        },
        _ => None,
      })
    })
    .collect();

  assert_eq!(markers, vec![3, 10, 9]);
}

#[test]
fn reversed_list_ignores_nested_items_for_default_start() {
  let mut ol_style = ComputedStyle::default();
  ol_style.display = Display::Block;
  let ol_style = Arc::new(ol_style);

  let mut li_style = ComputedStyle::default();
  li_style.display = Display::ListItem;
  li_style.list_style_type = ListStyleType::Decimal;
  let li_style = Arc::new(li_style);

  let nested_ol = StyledNode {
    node_id: 0,
    subtree_size: 1,
    node: dom::DomNode {
      node_type: dom::DomNodeType::Element {
        tag_name: "ol".to_string(),
        namespace: HTML_NAMESPACE.to_string(),
        attributes: vec![("reversed".to_string(), String::new())],
      },
      children: vec![],
    },
    styles: ol_style.clone(),
    starting_styles: StartingStyleSet::default(),
    before_styles: None,
    after_styles: None,
    marker_styles: None,
    placeholder_styles: None,
    file_selector_button_styles: None,
    footnote_call_styles: None,
    footnote_marker_styles: None,
    first_line_styles: None,
    first_letter_styles: None,
    slider_thumb_styles: None,
    slider_track_styles: None,
    progress_bar_styles: None,
    progress_value_styles: None,
    meter_bar_styles: None,
    meter_optimum_value_styles: None,
    meter_suboptimum_value_styles: None,
    meter_even_less_good_value_styles: None,
    assigned_slot: None,
    slotted_node_ids: Vec::new(),
    children: vec![StyledNode {
      node_id: 0,
      subtree_size: 1,
      node: dom::DomNode {
        node_type: dom::DomNodeType::Element {
          tag_name: "li".to_string(),
          namespace: HTML_NAMESPACE.to_string(),
          attributes: vec![],
        },
        children: vec![dom::DomNode {
          node_type: dom::DomNodeType::Text {
            content: "inner".to_string(),
          },
          children: vec![],
        }],
      },
      styles: li_style.clone(),
      starting_styles: StartingStyleSet::default(),
      before_styles: None,
      after_styles: None,
      marker_styles: None,
      placeholder_styles: None,
      file_selector_button_styles: None,
      footnote_call_styles: None,
      footnote_marker_styles: None,
      first_line_styles: None,
      first_letter_styles: None,
      slider_thumb_styles: None,
      slider_track_styles: None,
      progress_bar_styles: None,
      progress_value_styles: None,
      meter_bar_styles: None,
      meter_optimum_value_styles: None,
      meter_suboptimum_value_styles: None,
      meter_even_less_good_value_styles: None,
      assigned_slot: None,
      slotted_node_ids: Vec::new(),
      children: vec![StyledNode {
        node_id: 0,
        subtree_size: 1,
        node: dom::DomNode {
          node_type: dom::DomNodeType::Text {
            content: "inner".to_string(),
          },
          children: vec![],
        },
        styles: default_style(),
        starting_styles: StartingStyleSet::default(),
        before_styles: None,
        after_styles: None,
        marker_styles: None,
        placeholder_styles: None,
        file_selector_button_styles: None,
        footnote_call_styles: None,
        footnote_marker_styles: None,
        first_line_styles: None,
        first_letter_styles: None,
        slider_thumb_styles: None,
        slider_track_styles: None,
        progress_bar_styles: None,
        progress_value_styles: None,
        meter_bar_styles: None,
        meter_optimum_value_styles: None,
        meter_suboptimum_value_styles: None,
        meter_even_less_good_value_styles: None,
        assigned_slot: None,
        slotted_node_ids: Vec::new(),
        children: vec![],
      }],
    }],
  };

  let ol_dom = dom::DomNode {
    node_type: dom::DomNodeType::Element {
      tag_name: "ol".to_string(),
      namespace: HTML_NAMESPACE.to_string(),
      attributes: vec![("reversed".to_string(), String::new())],
    },
    children: vec![],
  };

  let outer_li = |child: StyledNode| StyledNode {
    node_id: 0,
    subtree_size: 1,
    node: dom::DomNode {
      node_type: dom::DomNodeType::Element {
        tag_name: "li".to_string(),
        namespace: HTML_NAMESPACE.to_string(),
        attributes: vec![],
      },
      children: vec![child.node.clone()],
    },
    styles: li_style.clone(),
    starting_styles: StartingStyleSet::default(),
    before_styles: None,
    after_styles: None,
    marker_styles: None,
    placeholder_styles: None,
    file_selector_button_styles: None,
    footnote_call_styles: None,
    footnote_marker_styles: None,
    first_line_styles: None,
    first_letter_styles: None,
    slider_thumb_styles: None,
    slider_track_styles: None,
    progress_bar_styles: None,
    progress_value_styles: None,
    meter_bar_styles: None,
    meter_optimum_value_styles: None,
    meter_suboptimum_value_styles: None,
    meter_even_less_good_value_styles: None,
    assigned_slot: None,
    slotted_node_ids: Vec::new(),
    children: vec![child],
  };

  let ol = StyledNode {
    node_id: 0,
    subtree_size: 1,
    node: ol_dom,
    styles: ol_style.clone(),
    starting_styles: StartingStyleSet::default(),
    before_styles: None,
    after_styles: None,
    marker_styles: None,
    placeholder_styles: None,
    file_selector_button_styles: None,
    footnote_call_styles: None,
    footnote_marker_styles: None,
    first_line_styles: None,
    first_letter_styles: None,
    slider_thumb_styles: None,
    slider_track_styles: None,
    progress_bar_styles: None,
    progress_value_styles: None,
    meter_bar_styles: None,
    meter_optimum_value_styles: None,
    meter_suboptimum_value_styles: None,
    meter_even_less_good_value_styles: None,
    assigned_slot: None,
    slotted_node_ids: Vec::new(),
    children: vec![
      outer_li(StyledNode {
        node_id: 0,
        subtree_size: 1,
        node: dom::DomNode {
          node_type: dom::DomNodeType::Text {
            content: "outer1".to_string(),
          },
          children: vec![],
        },
        styles: default_style(),
        starting_styles: StartingStyleSet::default(),
        before_styles: None,
        after_styles: None,
        marker_styles: None,
        placeholder_styles: None,
        file_selector_button_styles: None,
        footnote_call_styles: None,
        footnote_marker_styles: None,
        first_line_styles: None,
        first_letter_styles: None,
        slider_thumb_styles: None,
        slider_track_styles: None,
        progress_bar_styles: None,
        progress_value_styles: None,
        meter_bar_styles: None,
        meter_optimum_value_styles: None,
        meter_suboptimum_value_styles: None,
        meter_even_less_good_value_styles: None,
        assigned_slot: None,
        slotted_node_ids: Vec::new(),
        children: vec![],
      }),
      outer_li(nested_ol),
    ],
  };

  let tree = generate_box_tree(&ol);
  let markers: Vec<i32> = tree
    .root
    .children
    .iter()
    .filter_map(|li| {
      li.children.first().and_then(|child| match &child.box_type {
        BoxType::Marker(m) => match &m.content {
          MarkerContent::Text(t) => Some(marker_leading_decimal(t)),
          MarkerContent::Image(_) => None,
        },
        _ => None,
      })
    })
    .collect();

  assert_eq!(markers, vec![2, 1]);
}

#[test]
fn reversed_list_skips_menu_items_for_default_start() {
  let mut ol_style = ComputedStyle::default();
  ol_style.display = Display::Block;
  ol_style.list_style_type = ListStyleType::Decimal;
  let ol_style = Arc::new(ol_style);

  let mut li_style = ComputedStyle::default();
  li_style.display = Display::ListItem;
  li_style.list_style_type = ListStyleType::Decimal;
  let li_style = Arc::new(li_style);

  let mut menu_style = ComputedStyle::default();
  menu_style.display = Display::Block;
  menu_style.list_style_type = ListStyleType::Disc;
  let menu_style = Arc::new(menu_style);

  let text_node = |content: &str| StyledNode {
    node_id: 0,
    subtree_size: 1,
    node: dom::DomNode {
      node_type: dom::DomNodeType::Text {
        content: content.to_string(),
      },
      children: vec![],
    },
    styles: default_style(),
    starting_styles: StartingStyleSet::default(),
    before_styles: None,
    after_styles: None,
    marker_styles: None,
    placeholder_styles: None,
    file_selector_button_styles: None,
    footnote_call_styles: None,
    footnote_marker_styles: None,
    first_line_styles: None,
    first_letter_styles: None,
    slider_thumb_styles: None,
    slider_track_styles: None,
    progress_bar_styles: None,
    progress_value_styles: None,
    meter_bar_styles: None,
    meter_optimum_value_styles: None,
    meter_suboptimum_value_styles: None,
    meter_even_less_good_value_styles: None,
    assigned_slot: None,
    slotted_node_ids: Vec::new(),
    children: vec![],
  };

  let li = |content: &str| StyledNode {
    node_id: 0,
    subtree_size: 1,
    node: dom::DomNode {
      node_type: dom::DomNodeType::Element {
        tag_name: "li".to_string(),
        namespace: HTML_NAMESPACE.to_string(),
        attributes: vec![],
      },
      children: vec![],
    },
    styles: li_style.clone(),
    starting_styles: StartingStyleSet::default(),
    before_styles: None,
    after_styles: None,
    marker_styles: None,
    placeholder_styles: None,
    file_selector_button_styles: None,
    footnote_call_styles: None,
    footnote_marker_styles: None,
    first_line_styles: None,
    first_letter_styles: None,
    slider_thumb_styles: None,
    slider_track_styles: None,
    progress_bar_styles: None,
    progress_value_styles: None,
    meter_bar_styles: None,
    meter_optimum_value_styles: None,
    meter_suboptimum_value_styles: None,
    meter_even_less_good_value_styles: None,
    assigned_slot: None,
    slotted_node_ids: Vec::new(),
    children: vec![text_node(content)],
  };

  let menu = StyledNode {
    node_id: 0,
    subtree_size: 1,
    node: dom::DomNode {
      node_type: dom::DomNodeType::Element {
        tag_name: "menu".to_string(),
        namespace: HTML_NAMESPACE.to_string(),
        attributes: vec![],
      },
      children: vec![],
    },
    styles: menu_style,
    starting_styles: StartingStyleSet::default(),
    before_styles: None,
    after_styles: None,
    marker_styles: None,
    placeholder_styles: None,
    file_selector_button_styles: None,
    footnote_call_styles: None,
    footnote_marker_styles: None,
    first_line_styles: None,
    first_letter_styles: None,
    slider_thumb_styles: None,
    slider_track_styles: None,
    progress_bar_styles: None,
    progress_value_styles: None,
    meter_bar_styles: None,
    meter_optimum_value_styles: None,
    meter_suboptimum_value_styles: None,
    meter_even_less_good_value_styles: None,
    assigned_slot: None,
    slotted_node_ids: Vec::new(),
    children: vec![li("skip-1"), li("skip-2")],
  };

  let ol = StyledNode {
    node_id: 0,
    subtree_size: 1,
    node: dom::DomNode {
      node_type: dom::DomNodeType::Element {
        tag_name: "ol".to_string(),
        namespace: HTML_NAMESPACE.to_string(),
        attributes: vec![("reversed".to_string(), String::new())],
      },
      children: vec![],
    },
    styles: ol_style,
    starting_styles: StartingStyleSet::default(),
    before_styles: None,
    after_styles: None,
    marker_styles: None,
    placeholder_styles: None,
    file_selector_button_styles: None,
    footnote_call_styles: None,
    footnote_marker_styles: None,
    first_line_styles: None,
    first_letter_styles: None,
    slider_thumb_styles: None,
    slider_track_styles: None,
    progress_bar_styles: None,
    progress_value_styles: None,
    meter_bar_styles: None,
    meter_optimum_value_styles: None,
    meter_suboptimum_value_styles: None,
    meter_even_less_good_value_styles: None,
    assigned_slot: None,
    slotted_node_ids: Vec::new(),
    children: vec![li("one"), menu, li("two")],
  };

  let tree = generate_box_tree(&ol);
  let markers: Vec<i32> = tree
    .root
    .children
    .iter()
    .filter_map(|child| match &child.box_type {
      BoxType::Block(_) => child.children.first().and_then(|c| match &c.box_type {
        BoxType::Marker(m) => match &m.content {
          MarkerContent::Text(t) => Some(marker_leading_decimal(t)),
          MarkerContent::Image(_) => None,
        },
        _ => None,
      }),
      _ => None,
    })
    .collect();

  // Only top-level list items should contribute to the count: markers 2, 1.
  assert_eq!(markers, vec![2, 1]);
}

#[test]
fn nested_list_resets_increment_to_default() {
  // Outer reversed list should not force inner list to count down; inner list starts at 1.
  let mut ol_style = ComputedStyle::default();
  ol_style.display = Display::Block;
  ol_style.list_style_type = ListStyleType::Decimal;
  let ol_style = Arc::new(ol_style);

  let mut li_style = ComputedStyle::default();
  li_style.display = Display::ListItem;
  li_style.list_style_type = ListStyleType::Decimal;
  let li_style = Arc::new(li_style);

  let text_node = |content: &str| StyledNode {
    node_id: 0,
    subtree_size: 1,
    node: dom::DomNode {
      node_type: dom::DomNodeType::Text {
        content: content.to_string(),
      },
      children: vec![],
    },
    styles: default_style(),
    starting_styles: StartingStyleSet::default(),
    before_styles: None,
    after_styles: None,
    marker_styles: None,
    placeholder_styles: None,
    file_selector_button_styles: None,
    footnote_call_styles: None,
    footnote_marker_styles: None,
    first_line_styles: None,
    first_letter_styles: None,
    slider_thumb_styles: None,
    slider_track_styles: None,
    progress_bar_styles: None,
    progress_value_styles: None,
    meter_bar_styles: None,
    meter_optimum_value_styles: None,
    meter_suboptimum_value_styles: None,
    meter_even_less_good_value_styles: None,
    assigned_slot: None,
    slotted_node_ids: Vec::new(),
    children: vec![],
  };

  let mk_li = |content: &str| StyledNode {
    node_id: 0,
    subtree_size: 1,
    node: dom::DomNode {
      node_type: dom::DomNodeType::Element {
        tag_name: "li".to_string(),
        namespace: HTML_NAMESPACE.to_string(),
        attributes: vec![],
      },
      children: vec![],
    },
    styles: li_style.clone(),
    starting_styles: StartingStyleSet::default(),
    before_styles: None,
    after_styles: None,
    marker_styles: None,
    placeholder_styles: None,
    file_selector_button_styles: None,
    footnote_call_styles: None,
    footnote_marker_styles: None,
    first_line_styles: None,
    first_letter_styles: None,
    slider_thumb_styles: None,
    slider_track_styles: None,
    progress_bar_styles: None,
    progress_value_styles: None,
    meter_bar_styles: None,
    meter_optimum_value_styles: None,
    meter_suboptimum_value_styles: None,
    meter_even_less_good_value_styles: None,
    assigned_slot: None,
    slotted_node_ids: Vec::new(),
    children: vec![text_node(content)],
  };

  let inner_ol = StyledNode {
    node_id: 0,
    subtree_size: 1,
    node: dom::DomNode {
      node_type: dom::DomNodeType::Element {
        tag_name: "ol".to_string(),
        namespace: HTML_NAMESPACE.to_string(),
        attributes: vec![],
      },
      children: vec![],
    },
    styles: ol_style.clone(),
    starting_styles: StartingStyleSet::default(),
    before_styles: None,
    after_styles: None,
    marker_styles: None,
    placeholder_styles: None,
    file_selector_button_styles: None,
    footnote_call_styles: None,
    footnote_marker_styles: None,
    first_line_styles: None,
    first_letter_styles: None,
    slider_thumb_styles: None,
    slider_track_styles: None,
    progress_bar_styles: None,
    progress_value_styles: None,
    meter_bar_styles: None,
    meter_optimum_value_styles: None,
    meter_suboptimum_value_styles: None,
    meter_even_less_good_value_styles: None,
    assigned_slot: None,
    slotted_node_ids: Vec::new(),
    children: vec![mk_li("inner-one"), mk_li("inner-two")],
  };

  let outer_li = StyledNode {
    node_id: 0,
    subtree_size: 1,
    node: dom::DomNode {
      node_type: dom::DomNodeType::Element {
        tag_name: "li".to_string(),
        namespace: HTML_NAMESPACE.to_string(),
        attributes: vec![],
      },
      children: vec![],
    },
    styles: li_style.clone(),
    starting_styles: StartingStyleSet::default(),
    before_styles: None,
    after_styles: None,
    marker_styles: None,
    placeholder_styles: None,
    file_selector_button_styles: None,
    footnote_call_styles: None,
    footnote_marker_styles: None,
    first_line_styles: None,
    first_letter_styles: None,
    slider_thumb_styles: None,
    slider_track_styles: None,
    progress_bar_styles: None,
    progress_value_styles: None,
    meter_bar_styles: None,
    meter_optimum_value_styles: None,
    meter_suboptimum_value_styles: None,
    meter_even_less_good_value_styles: None,
    assigned_slot: None,
    slotted_node_ids: Vec::new(),
    children: vec![text_node("outer"), inner_ol],
  };

  let outer_ol = StyledNode {
    node_id: 0,
    subtree_size: 1,
    node: dom::DomNode {
      node_type: dom::DomNodeType::Element {
        tag_name: "ol".to_string(),
        namespace: HTML_NAMESPACE.to_string(),
        attributes: vec![("reversed".to_string(), String::new())],
      },
      children: vec![],
    },
    styles: ol_style,
    starting_styles: StartingStyleSet::default(),
    before_styles: None,
    after_styles: None,
    marker_styles: None,
    placeholder_styles: None,
    file_selector_button_styles: None,
    footnote_call_styles: None,
    footnote_marker_styles: None,
    first_line_styles: None,
    first_letter_styles: None,
    slider_thumb_styles: None,
    slider_track_styles: None,
    progress_bar_styles: None,
    progress_value_styles: None,
    meter_bar_styles: None,
    meter_optimum_value_styles: None,
    meter_suboptimum_value_styles: None,
    meter_even_less_good_value_styles: None,
    assigned_slot: None,
    slotted_node_ids: Vec::new(),
    children: vec![outer_li],
  };

  let tree = generate_box_tree(&outer_ol);
  let inner_box = tree.root.children[0]
    .children
    .iter()
    .find(|child| matches!(child.box_type, BoxType::Block(_)))
    .expect("inner list block");

  let first_inner_marker = match &inner_box.children[0].children[0].box_type {
    BoxType::Marker(marker) => marker.content.clone(),
    other => panic!("expected marker for first inner item, got {:?}", other),
  };
  let second_inner_marker = match &inner_box.children[1].children[0].box_type {
    BoxType::Marker(marker) => marker.content.clone(),
    other => panic!("expected marker for second inner item, got {:?}", other),
  };

  let first_text = match first_inner_marker {
    MarkerContent::Text(t) => t,
    MarkerContent::Image(_) => panic!("expected text marker, got Image"),
  };
  let second_text = match second_inner_marker {
    MarkerContent::Text(t) => t,
    MarkerContent::Image(_) => panic!("expected text marker, got Image"),
  };

  assert_eq!(first_text, "1. ");
  assert_eq!(second_text, "2. ");
}

#[test]
fn inline_svg_carries_serialized_content() {
  let html = r#"<html><body><svg width="10" height="10"><rect width="10" height="10" fill="red"/></svg></body></html>"#;
  let dom = crate::dom::parse_html(html).expect("parse");
  let styled = crate::style::cascade::apply_styles(&dom, &crate::css::types::StyleSheet::new());
  let box_tree = generate_box_tree(&styled);

  fn find_svg(node: &BoxNode) -> Option<&ReplacedBox> {
    if let BoxType::Replaced(repl) = &node.box_type {
      if matches!(repl.replaced_type, ReplacedType::Svg { .. }) {
        return Some(repl);
      }
    }
    for child in node.children.iter() {
      if let Some(found) = find_svg(child) {
        return Some(found);
      }
    }
    None
  }

  let svg = find_svg(&box_tree.root).expect("svg replaced box");
  match &svg.replaced_type {
    ReplacedType::Svg { content } => {
      assert!(
        content.svg.contains("<rect"),
        "serialized SVG should include child elements (svg={})",
        content.svg
      );
    }
    other => panic!("expected svg replaced type, got {:?}", other),
  }
}

#[test]
fn svg_document_css_forced_on_injects_even_without_class_or_id() {
  let html = r#"
    <style>rect { fill: red; }</style>
    <svg width="10" height="10" viewBox="0 0 10 10"><rect width="10" height="10"/></svg>
  "#;

  let content = runtime::with_runtime_toggles(
    Arc::new(RuntimeToggles::from_map(HashMap::from([(
      "FASTR_SVG_EMBED_DOCUMENT_CSS".to_string(),
      "1".to_string(),
    )]))),
    || serialized_inline_svg_content_from_html(html, 20.0, 20.0).expect("serialize svg"),
  );

  assert!(
    content.document_css_injection.is_some(),
    "document CSS injection should happen when forced on even without class/id in the SVG subtree"
  );
}

#[test]
fn svg_document_css_embedding_policy_respects_svg_count_overrides_and_size_limit() {
  let html_many_svgs = r#"
    <style>
      svg.icon .shape { fill: red; }
    </style>
    <svg class="icon" width="10" height="10" viewBox="0 0 10 10">
      <rect class="shape" width="10" height="10" />
    </svg>
    <svg class="icon" width="10" height="10" viewBox="0 0 10 10">
      <rect class="shape" width="10" height="10" />
    </svg>
  "#;

  // When the document contains more replaced SVGs than allowed (and embedding is not forced),
  // skip embedding document CSS to avoid O(svg_count × css_bytes) blowups.
  let disabled = runtime::with_runtime_toggles(
    Arc::new(RuntimeToggles::from_map(HashMap::from([(
      "FASTR_SVG_EMBED_DOCUMENT_CSS_MAX_SVGS".to_string(),
      "1".to_string(),
    )]))),
    || serialized_inline_svg_content_from_html(html_many_svgs, 20.0, 20.0).expect("serialize svg"),
  );
  assert!(
    disabled.document_css_injection.is_none(),
    "document CSS embedding should be disabled when SVG count exceeds the max"
  );
  assert!(
    disabled.fallback_svg.is_empty(),
    "fallback SVG should remain empty when document CSS embedding is disabled"
  );

  // Forcing embedding on should override the SVG count guard (while still honoring the CSS size cap).
  let forced_on = runtime::with_runtime_toggles(
    Arc::new(RuntimeToggles::from_map(HashMap::from([
      ("FASTR_SVG_EMBED_DOCUMENT_CSS".to_string(), "1".to_string()),
      (
        "FASTR_SVG_EMBED_DOCUMENT_CSS_MAX_SVGS".to_string(),
        "1".to_string(),
      ),
    ]))),
    || serialized_inline_svg_content_from_html(html_many_svgs, 20.0, 20.0).expect("serialize svg"),
  );
  assert!(
    forced_on.document_css_injection.is_some(),
    "document CSS should be embedded when forced on"
  );
  assert!(
    !forced_on.svg.contains("<style><![CDATA["),
    "document CSS should be injected at render time instead of being inlined into every SVG"
  );
  assert_eq!(
    forced_on.fallback_svg, "",
    "fallback SVG should remain empty for non-foreignObject inline SVGs"
  );

  // Forcing embedding off should disable it even for a single inline SVG.
  let html_one_svg = r#"
    <style>
      svg.icon .shape { fill: red; }
    </style>
    <svg class="icon" width="10" height="10" viewBox="0 0 10 10">
      <rect class="shape" width="10" height="10" />
    </svg>
  "#;
  let forced_off = runtime::with_runtime_toggles(
    Arc::new(RuntimeToggles::from_map(HashMap::from([(
      "FASTR_SVG_EMBED_DOCUMENT_CSS".to_string(),
      "0".to_string(),
    )]))),
    || serialized_inline_svg_content_from_html(html_one_svg, 20.0, 20.0).expect("serialize svg"),
  );
  assert!(
    forced_off.document_css_injection.is_none(),
    "document CSS embedding should be disabled when forced off"
  );
  assert!(
    forced_off.fallback_svg.is_empty(),
    "fallback SVG should remain empty when forced off"
  );

  // The embed override must still honor the 64KiB embedded CSS cap.
  let oversized_css = ".x{fill:red;}\n".repeat(5000);
  let html_oversized_css = format!(
    "<style>{}</style><svg class=\"icon\" width=\"10\" height=\"10\" viewBox=\"0 0 10 10\"><rect class=\"shape\" width=\"10\" height=\"10\" /></svg>",
    oversized_css
  );
  let forced_on_oversized = runtime::with_runtime_toggles(
    Arc::new(RuntimeToggles::from_map(HashMap::from([(
      "FASTR_SVG_EMBED_DOCUMENT_CSS".to_string(),
      "1".to_string(),
    )]))),
    || serialized_inline_svg_content_from_html(&html_oversized_css, 20.0, 20.0).expect("serialize svg"),
  );
  assert!(
    forced_on_oversized.document_css_injection.is_none(),
    "document CSS should not be embedded when it exceeds the embedded CSS cap"
  );
  assert!(
    forced_on_oversized.fallback_svg.is_empty(),
    "fallback SVG should remain empty when embedding is disabled due to size limit"
  );
}

#[test]
fn inline_svg_inlines_svg_rendering_properties_from_document_css_when_embedding_disabled() {
  let html = r#"
    <style>
      svg .shape {
        shape-rendering: CRISPEDGES;
        vector-effect: Non-Scaling-Stroke;
        color-rendering: optimizequality;
        color-interpolation: linearrgb;
        color-interpolation-filters: sRgb;
      }
      svg mask {
        mask-type: ALPHA;
      }
    </style>
    <svg width="10" height="10" viewBox="0 0 10 10">
      <defs>
        <mask id="m">
          <rect width="10" height="10" fill="white" />
        </mask>
      </defs>
      <rect class="shape" width="10" height="10" mask="url(#m)" />
    </svg>
  "#;

  let content = runtime::with_runtime_toggles(
    Arc::new(RuntimeToggles::from_map(HashMap::from([(
      "FASTR_SVG_EMBED_DOCUMENT_CSS".to_string(),
      "0".to_string(),
    )]))),
    || serialized_inline_svg_content_from_html(html, 20.0, 20.0).expect("serialize svg"),
  );

  assert!(
    content.svg.contains("shape-rendering: crispEdges"),
    "shape-rendering should be inlined with canonical casing (svg={})",
    content.svg
  );
  assert!(
    content.svg.contains("vector-effect: non-scaling-stroke"),
    "vector-effect should be inlined with canonical casing (svg={})",
    content.svg
  );
  assert!(
    content.svg.contains("color-rendering: optimizeQuality"),
    "color-rendering should be inlined with canonical casing (svg={})",
    content.svg
  );
  assert!(
    content.svg.contains("color-interpolation: linearRGB"),
    "color-interpolation should be inlined with canonical casing (svg={})",
    content.svg
  );
  assert!(
    content.svg.contains("color-interpolation-filters: sRGB"),
    "color-interpolation-filters should be inlined with canonical casing (svg={})",
    content.svg
  );
  assert!(
    content.svg.contains("mask-type: alpha"),
    "mask-type should be inlined with canonical casing (svg={})",
    content.svg
  );
}

#[test]
fn inline_svg_inlines_svg_rendering_properties_from_presentation_attributes() {
  let html = r#"
    <svg width="10" height="10" viewBox="0 0 10 10">
      <defs>
        <mask id="m" mask-type="alpha">
          <rect width="10" height="10" fill="white" />
        </mask>
      </defs>
      <rect
        width="10"
        height="10"
        mask="url(#m)"
        shape-rendering="crispEdges"
        vector-effect="non-scaling-stroke"
        color-rendering="optimizeQuality"
        color-interpolation="linearRGB"
        color-interpolation-filters="sRGB"
      />
    </svg>
  "#;

  let content = runtime::with_runtime_toggles(
    Arc::new(RuntimeToggles::from_map(HashMap::from([(
      "FASTR_SVG_EMBED_DOCUMENT_CSS".to_string(),
      "0".to_string(),
    )]))),
    || serialized_inline_svg_content_from_html(html, 20.0, 20.0).expect("serialize svg"),
  );

  assert!(
    content.svg.contains("shape-rendering: crispEdges"),
    "presentation attribute should be inlined into style (svg={})",
    content.svg
  );
  assert!(
    content.svg.contains("vector-effect: non-scaling-stroke"),
    "presentation attribute should be inlined into style (svg={})",
    content.svg
  );
  assert!(
    content.svg.contains("color-rendering: optimizeQuality"),
    "presentation attribute should be inlined into style (svg={})",
    content.svg
  );
  assert!(
    content.svg.contains("color-interpolation: linearRGB"),
    "presentation attribute should be inlined into style (svg={})",
    content.svg
  );
  assert!(
    content.svg.contains("color-interpolation-filters: sRGB"),
    "presentation attribute should be inlined into style (svg={})",
    content.svg
  );
  assert!(
    content.svg.contains("mask-type: alpha"),
    "presentation attribute should be inlined into style (svg={})",
    content.svg
  );

  assert!(
    !content.svg.contains("shape-rendering=\""),
    "authored presentation attributes should be stripped once we inline computed styles (svg={})",
    content.svg
  );
  assert!(
    !content.svg.contains("vector-effect=\""),
    "authored presentation attributes should be stripped once we inline computed styles (svg={})",
    content.svg
  );
  assert!(
    !content.svg.contains("color-rendering=\""),
    "authored presentation attributes should be stripped once we inline computed styles (svg={})",
    content.svg
  );
  assert!(
    !content.svg.contains("color-interpolation=\""),
    "authored presentation attributes should be stripped once we inline computed styles (svg={})",
    content.svg
  );
  assert!(
    !content.svg.contains("color-interpolation-filters=\""),
    "authored presentation attributes should be stripped once we inline computed styles (svg={})",
    content.svg
  );
  assert!(
    !content.svg.contains("mask-type=\""),
    "authored presentation attributes should be stripped once we inline computed styles (svg={})",
    content.svg
  );
}

#[test]
fn inline_svg_malformed_style_attribute_is_stripped_before_inlined_presentation_styles() {
  let html = r#"
  <style>
    body { margin: 0; background: white; }
    svg { display: block; }
  </style>
  <svg width="20" height="20" viewBox="0 0 20 20">
    <g fill="none" stroke="none" style="fill: var(--c;">
      <rect width="20" height="20"></rect>
    </g>
  </svg>
  "#;

  let content = serialized_inline_svg_content_from_html(html, 30.0, 30.0).expect("svg content");
  assert!(
    !content.svg.contains("var(--c"),
    "serialized SVG should drop malformed authored style before appending computed declarations; got: {}",
    content.svg
  );
  assert!(
    content.svg.contains("fill: none"),
    "serialized SVG should include computed fill: none; got: {}",
    content.svg
  );
}

#[test]
fn inline_svg_serialization_preserves_mask_attributes_and_mask_affects_alpha_in_standalone_rendering(
) {
  use crate::image_loader::ImageCache;

  let html = r#"
  <style>
    body { margin: 0; background: white; }
    svg { display: block; }
  </style>
  <svg width="20" height="20" viewBox="0 0 20 20" style="display: block">
    <defs>
      <linearGradient id="grad" x1="0" x2="1" y1="0" y2="0">
        <stop offset="0%" stop-color="red" />
        <stop offset="100%" stop-color="blue" />
      </linearGradient>
      <mask id="fade">
        <rect width="20" height="20" fill="white" />
        <rect width="10" height="20" fill="black" />
      </mask>
    </defs>
    <rect width="20" height="20" fill="url(#grad)" mask="url(#fade)" />
    <rect width="4" height="4" fill="black" transform="translate(12 2)" />
  </svg>
  "#;

  let serialized = serialized_inline_svg_content_from_html(html, 30.0, 30.0).expect("serialize svg");
  assert!(
    serialized.svg.contains("mask=\"url(#fade)\""),
    "serialized SVG should preserve mask attributes (svg={})",
    serialized.svg
  );

  let cache = ImageCache::new();
  let svg_image = cache
    .render_svg(&serialized.svg)
    .expect("render serialized svg");
  let svg_rgba = svg_image.image.to_rgba8();
  let left_alpha = svg_rgba.get_pixel(8, 10)[3];
  let right_alpha = svg_rgba.get_pixel(14, 10)[3];
  assert!(
    left_alpha < right_alpha,
    "mask should reduce alpha in standalone rendering"
  );
}

#[test]
fn inline_svg_wraps_document_css_in_cdata() {
  use crate::debug::runtime;
  use roxmltree::Document;

  let html = r#"
  <style>
    svg .shape {
      background-image: url("data:image/svg+xml,<svg xmlns='http://www.w3.org/2000/svg'><rect width='1' height='1'/></svg>?a&b]]>");
    }
  </style>
  <svg width="10" height="10" viewBox="0 0 10 10">
    <rect class="shape" width="10" height="10" />
  </svg>
  "#;

  let serialized = runtime::with_runtime_toggles(
    Arc::new(RuntimeToggles::from_map(HashMap::from([(
      "FASTR_SVG_EMBED_DOCUMENT_CSS".to_string(),
      "1".to_string(),
    )]))),
    || serialized_inline_svg_content_from_html(html, 20.0, 20.0).expect("serialize svg"),
  );
  let injection = serialized
    .document_css_injection
    .as_ref()
    .expect("document CSS injection should be captured");
  assert!(
    injection.style_element.contains("<![CDATA["),
    "embedded document CSS should be wrapped in CDATA"
  );
  assert!(
    injection.style_element.contains("]]]]><![CDATA[>"),
    "]]> terminators inside CSS should be split across CDATA sections"
  );

  let mut svg_with_css = String::with_capacity(serialized.svg.len() + injection.style_element.len());
  svg_with_css.push_str(&serialized.svg[..injection.insert_pos]);
  svg_with_css.push_str(injection.style_element.as_ref());
  svg_with_css.push_str(&serialized.svg[injection.insert_pos..]);

  Document::parse(&serialized.svg).expect("serialized svg should be parseable XML");
  let doc = Document::parse(&svg_with_css).expect("parse serialized svg with injected CSS");
  let style_text = doc
    .descendants()
    .find(|n| n.is_element() && n.tag_name().name() == "style")
    .map(|n| {
      n.descendants()
        .filter(|t| t.is_text())
        .filter_map(|t| t.text())
        .collect::<String>()
    })
    .expect("style element text");
  assert!(
    style_text.contains("background-image"),
    "style text should be preserved after CDATA wrapping"
  );
  assert!(
    style_text.contains("]]>"),
    "original CSS content containing CDATA terminators should round-trip"
  );

  let cache = crate::image_loader::ImageCache::new();
  cache
    .render_svg(&svg_with_css)
    .expect("render serialized svg with CDATA-wrapped CSS");
}

#[test]
fn foreign_object_shared_css_respects_limit() {
  fn pixel(pixmap: &tiny_skia::Pixmap, x: u32, y: u32) -> [u8; 4] {
    let idx = (y as usize * pixmap.width() as usize + x as usize) * 4;
    let data = pixmap.data();
    [data[idx], data[idx + 1], data[idx + 2], data[idx + 3]]
  }

  std::thread::Builder::new()
    .stack_size(64 * 1024 * 1024)
    .spawn(|| {
      let large_css = "body { color: black; }\n".repeat(20_000);
      let html = format!(
        r#"
        <style>
          body {{ margin: 0; background: white; }}
          {}
        </style>
        <svg width="32" height="16" viewBox="0 0 32 16">
          <foreignObject x="0" y="0" width="16" height="16">
            <div xmlns="http://www.w3.org/1999/xhtml" style="width:16px;height:16px;background: rgb(255, 0, 0);"></div>
          </foreignObject>
        </svg>
        "#,
        large_css
      );

      let serialized =
        serialized_inline_svg_content_from_html(&html, 32.0, 16.0).expect("serialize svg with foreignObject");
      assert!(
        serialized.shared_css.is_empty(),
        "shared CSS should be dropped when it exceeds the limit (len={})",
        large_css.len()
      );

      let mut renderer = crate::FastRender::new().expect("renderer");
      let pixmap = renderer
        .render_html(&html, 32, 16)
        .expect("render svg with large CSS");
      assert_eq!(pixel(&pixmap, 8, 8), [255, 0, 0, 255]);
    })
    .unwrap()
    .join()
    .unwrap();
}

#[test]
fn foreign_object_css_sanitizes_style_tag_sequences() {
  fn pixel(pixmap: &tiny_skia::Pixmap, x: u32, y: u32) -> [u8; 4] {
    let idx = (y as usize * pixmap.width() as usize + x as usize) * 4;
    let data = pixmap.data();
    [data[idx], data[idx + 1], data[idx + 2], data[idx + 3]]
  }

  std::thread::Builder::new()
    .stack_size(64 * 1024 * 1024)
    .spawn(|| {
      let html = r#"
      <body style="margin: 0">
        <svg width="16" height="16" viewBox="0 0 16 16">
          <style><![CDATA[
            /* stray </style> token that must not terminate the style element */
            .embed {
              width: 12px;
              height: 12px;
              background: rgb(0, 255, 0);
            }
          ]]></style>
          <foreignObject x="0" y="0" width="16" height="16">
            <div xmlns="http://www.w3.org/1999/xhtml" class="embed"></div>
          </foreignObject>
        </svg>
      </body>
      "#;

      let serialized =
        serialized_inline_svg_content_from_html(html, 16.0, 16.0).expect("serialize svg with foreignObject");
      assert!(
        !serialized.shared_css.is_empty(),
        "CSS under the limit should be preserved for foreignObject rendering"
      );
      assert!(
        serialized.shared_css.to_ascii_lowercase().contains("</style"),
        "shared CSS should retain literal </style> sequences for sanitization"
      );

      let mut renderer = crate::FastRender::new().expect("renderer");
      let pixmap = renderer
        .render_html(html, 16, 16)
        .expect("render foreignObject with sanitized CSS");
      assert_eq!(pixel(&pixmap, 8, 8), [0, 255, 0, 255]);
    })
    .unwrap()
    .join()
    .unwrap();
}

#[test]
fn foreign_object_background_from_svg_style_attribute_is_captured() {
  let html = r#"
  <svg width="16" height="16" viewBox="0 0 16 16">
    <foreignObject x="0" y="0" width="16" height="16" style="background: rgba(255, 0, 0, 0.5);">
      <div xmlns="http://www.w3.org/1999/xhtml" style="width:16px;height:16px;"></div>
    </foreignObject>
  </svg>
  "#;

  let serialized = serialized_inline_svg_content_from_html(html, 20.0, 20.0).expect("serialize svg");
  assert_eq!(
    serialized.foreign_objects.len(),
    1,
    "expected one foreignObject to be captured"
  );
  let bg = serialized.foreign_objects[0]
    .background
    .expect("foreignObject background should be captured");
  assert_eq!(bg.r, 255);
  assert_eq!(bg.g, 0);
  assert_eq!(bg.b, 0);
  assert!(
    (bg.a - 0.5).abs() < 0.01,
    "expected alpha ~0.5, got {:?}",
    bg
  );
}

#[test]
fn foreign_object_without_dimensions_uses_placeholder_comment() {
  let html = r#"
  <svg width="16" height="12" viewBox="0 0 16 12">
    <foreignObject>
      <div xmlns="http://www.w3.org/1999/xhtml" style="width:10px;height:12px;background: rgb(255, 0, 0);"></div>
    </foreignObject>
  </svg>
  "#;

  let serialized = serialized_inline_svg_content_from_html(html, 20.0, 20.0).expect("serialize svg");
  assert!(
    serialized
      .svg
      .contains("FASTRENDER_FOREIGN_OBJECT_UNRESOLVED"),
    "missing dimensions should keep placeholder path"
  );
}

#[test]
fn foreign_object_with_dimensions_emits_marker() {
  let html = r#"
  <svg width="16" height="12" viewBox="0 0 16 12">
    <foreignObject x="0" y="0" width="10" height="12">
      <div xmlns="http://www.w3.org/1999/xhtml" style="width:10px;height:12px;background: rgb(0, 255, 0);"></div>
    </foreignObject>
  </svg>
  "#;

  let serialized = serialized_inline_svg_content_from_html(html, 20.0, 20.0).expect("serialize svg");
  assert!(
    serialized.svg.contains("FASTRENDER_FOREIGN_OBJECT_0"),
    "foreignObject should be replaced with marker for nested rendering"
  );
  assert!(
    !serialized
      .svg
      .contains("FASTRENDER_FOREIGN_OBJECT_UNRESOLVED"),
    "valid dimensions should avoid unresolved placeholder comments"
  );
}

#[test]
fn foreign_object_display_none_does_not_emit_foreign_object_info() {
  let html = r#"
  <style>
    foreignObject{display:none}
  </style>
  <svg width="16" height="12" viewBox="0 0 16 12">
    <foreignObject x="0" y="0" width="10" height="12">
      <div xmlns="http://www.w3.org/1999/xhtml" style="width:10px;height:12px;background: rgb(0, 0, 255);"></div>
    </foreignObject>
  </svg>
  "#;

  let serialized = serialized_inline_svg_content_from_html(html, 20.0, 20.0).expect("serialize svg");
  assert!(
    serialized.foreign_objects.is_empty(),
    "display:none foreignObject should not allocate foreign object info"
  );
  assert!(
    !serialized.svg.contains("FASTRENDER_FOREIGN_OBJECT_0"),
    "display:none foreignObject should not emit a placeholder marker"
  );
}

#[test]
fn foreign_object_accepts_absolute_units_for_dimensions() {
  let html = r#"
  <svg width="2in" height="2in" viewBox="0 0 192 192">
    <foreignObject x="1in" y="0" width="1in" height="1in">
      <div xmlns="http://www.w3.org/1999/xhtml" style="width:96px;height:96px;background: rgb(0, 255, 0);"></div>
    </foreignObject>
  </svg>
  "#;

  let serialized = serialized_inline_svg_content_from_html(html, 200.0, 200.0).expect("serialize svg");
  assert!(
    serialized.svg.contains("FASTRENDER_FOREIGN_OBJECT_0"),
    "absolute units should resolve to a valid foreignObject"
  );
  assert!(
    !serialized
      .svg
      .contains("FASTRENDER_FOREIGN_OBJECT_UNRESOLVED"),
    "converted dimensions should avoid unresolved placeholder"
  );
}

#[test]
fn foreign_object_percentage_units_do_not_emit_unresolved_placeholder() {
  let html = r#"
  <svg width="16" height="12" viewBox="0 0 16 12">
    <foreignObject x="0" y="0" width="100%" height="100%">
      <div xmlns="http://www.w3.org/1999/xhtml" style="width:100%;height:100%;background: rgb(0, 0, 255);"></div>
    </foreignObject>
  </svg>
  "#;

  let serialized = serialized_inline_svg_content_from_html(html, 20.0, 20.0).expect("serialize svg");
  assert!(
    serialized.svg.contains("FASTRENDER_FOREIGN_OBJECT_0"),
    "percentage dimensions should resolve to a valid foreignObject"
  );
  assert!(
    !serialized
      .svg
      .contains("FASTRENDER_FOREIGN_OBJECT_UNRESOLVED"),
    "percentage dimensions should avoid unresolved placeholder comments"
  );
}

#[test]
fn deep_box_generation_does_not_overflow_stack() {
  use style::display::Display;

  let depth = 100_000usize;
  let mut computed = ComputedStyle::default();
  computed.display = Display::Block;
  let style = Arc::new(computed);

  let mut node = StyledNode {
    node_id: depth,
    subtree_size: 1,
    node: DomNode {
      node_type: DomNodeType::Element {
        tag_name: "div".to_string(),
        namespace: HTML_NAMESPACE.to_string(),
        attributes: vec![],
      },
      children: vec![],
    },
    styles: Arc::clone(&style),
    starting_styles: StartingStyleSet::default(),
    before_styles: None,
    after_styles: None,
    marker_styles: None,
    placeholder_styles: None,
    file_selector_button_styles: None,
    footnote_call_styles: None,
    footnote_marker_styles: None,
    first_line_styles: None,
    first_letter_styles: None,
    slider_thumb_styles: None,
    slider_track_styles: None,
    progress_bar_styles: None,
    progress_value_styles: None,
    meter_bar_styles: None,
    meter_optimum_value_styles: None,
    meter_suboptimum_value_styles: None,
    meter_even_less_good_value_styles: None,
    assigned_slot: None,
    slotted_node_ids: Vec::new(),
    children: vec![],
  };

  for node_id in (0..depth).rev() {
    let child = node;
    node = StyledNode {
      node_id,
      subtree_size: child.subtree_size.saturating_add(1),
      node: DomNode {
        node_type: DomNodeType::Element {
          tag_name: "div".to_string(),
          namespace: HTML_NAMESPACE.to_string(),
          attributes: vec![],
        },
        children: vec![],
      },
      styles: Arc::clone(&style),
      starting_styles: StartingStyleSet::default(),
      before_styles: None,
      after_styles: None,
      marker_styles: None,
      placeholder_styles: None,
      file_selector_button_styles: None,
      footnote_call_styles: None,
      footnote_marker_styles: None,
      first_line_styles: None,
      first_letter_styles: None,
      slider_thumb_styles: None,
      slider_track_styles: None,
      progress_bar_styles: None,
      progress_value_styles: None,
      meter_bar_styles: None,
      meter_optimum_value_styles: None,
      meter_suboptimum_value_styles: None,
      meter_even_less_good_value_styles: None,
      assigned_slot: None,
      slotted_node_ids: Vec::new(),
      children: vec![child],
    };
  }

  let _tree = generate_box_tree_with_anonymous_fixup_result(&node)
    .expect("deep box generation should not overflow stack");
}

#[test]
fn deep_select_form_control_generation_does_not_overflow_stack() {
  use style::display::Display;

  let depth = 100_000usize;
  let mut computed = ComputedStyle::default();
  computed.display = Display::InlineBlock;
  let style = Arc::new(computed);

  let mut option = StyledNode {
    node_id: depth + 1,
    subtree_size: 1,
    node: DomNode {
      node_type: DomNodeType::Element {
        tag_name: "option".to_string(),
        namespace: HTML_NAMESPACE.to_string(),
        attributes: vec![
          ("value".to_string(), "chosen".to_string()),
          ("label".to_string(), "chosen".to_string()),
          ("selected".to_string(), String::new()),
        ],
      },
      children: vec![],
    },
    styles: Arc::clone(&style),
    starting_styles: StartingStyleSet::default(),
    before_styles: None,
    after_styles: None,
    marker_styles: None,
    placeholder_styles: None,
    file_selector_button_styles: None,
    footnote_call_styles: None,
    footnote_marker_styles: None,
    first_line_styles: None,
    first_letter_styles: None,
    slider_thumb_styles: None,
    slider_track_styles: None,
    progress_bar_styles: None,
    progress_value_styles: None,
    meter_bar_styles: None,
    meter_optimum_value_styles: None,
    meter_suboptimum_value_styles: None,
    meter_even_less_good_value_styles: None,
    assigned_slot: None,
    slotted_node_ids: Vec::new(),
    children: vec![],
  };

  for node_id in (1..=depth).rev() {
    let child = option;
    option = StyledNode {
      node_id,
      subtree_size: child.subtree_size.saturating_add(1),
      node: DomNode {
        node_type: DomNodeType::Element {
          tag_name: "optgroup".to_string(),
          namespace: HTML_NAMESPACE.to_string(),
          attributes: vec![],
        },
        children: vec![],
      },
      styles: Arc::clone(&style),
      starting_styles: StartingStyleSet::default(),
      before_styles: None,
      after_styles: None,
      marker_styles: None,
      placeholder_styles: None,
      file_selector_button_styles: None,
      footnote_call_styles: None,
      footnote_marker_styles: None,
      first_line_styles: None,
      first_letter_styles: None,
      slider_thumb_styles: None,
      slider_track_styles: None,
      progress_bar_styles: None,
      progress_value_styles: None,
      meter_bar_styles: None,
      meter_optimum_value_styles: None,
      meter_suboptimum_value_styles: None,
      meter_even_less_good_value_styles: None,
      assigned_slot: None,
      slotted_node_ids: Vec::new(),
      children: vec![child],
    };
  }

  let select = StyledNode {
    node_id: 0,
    subtree_size: 1,
    node: DomNode {
      node_type: DomNodeType::Element {
        tag_name: "select".to_string(),
        namespace: HTML_NAMESPACE.to_string(),
        attributes: vec![("required".to_string(), String::new())],
      },
      children: vec![],
    },
    styles: Arc::clone(&style),
    starting_styles: StartingStyleSet::default(),
    before_styles: None,
    after_styles: None,
    marker_styles: None,
    placeholder_styles: None,
    file_selector_button_styles: None,
    footnote_call_styles: None,
    footnote_marker_styles: None,
    first_line_styles: None,
    first_letter_styles: None,
    slider_thumb_styles: None,
    slider_track_styles: None,
    progress_bar_styles: None,
    progress_value_styles: None,
    meter_bar_styles: None,
    meter_optimum_value_styles: None,
    meter_suboptimum_value_styles: None,
    meter_even_less_good_value_styles: None,
    assigned_slot: None,
    slotted_node_ids: Vec::new(),
    children: vec![option],
  };

  let tree = generate_box_tree(&select);
  let BoxType::Replaced(replaced) = &tree.root.box_type else {
    panic!("expected select to generate a replaced box");
  };
  let ReplacedType::FormControl(FormControl { control, .. }) = &replaced.replaced_type else {
    panic!("expected form control replaced type");
  };
  let FormControlKind::Select(select) = control else {
    panic!("expected select form control kind");
  };
  assert!(!select.multiple);
  assert!(select.selected.first().copied().is_some_and(|idx| matches!(
    select.items.get(idx),
    Some(SelectItem::Option { label, .. }) if label == "chosen"
  )));
}

#[test]
fn select_fallback_selects_first_option_when_all_disabled() {
  fn set_attr(node: &mut StyledNode, name: &str, value: &str) {
    match &mut node.node.node_type {
      DomNodeType::Element { attributes, .. } => {
        attributes.push((name.to_string(), value.to_string()));
      }
      _ => panic!("expected element node"),
    }
  }

  let mut first = styled_element("option");
  set_attr(&mut first, "disabled", "");
  set_attr(&mut first, "label", "First");
  set_attr(&mut first, "value", "a");

  let mut second = styled_element("option");
  set_attr(&mut second, "disabled", "");
  set_attr(&mut second, "label", "Second");
  set_attr(&mut second, "value", "b");

  let mut select = styled_element("select");
  select.children = vec![first, second];

  let control = create_form_control_replaced(&select, &[], None).expect("select form control");
  assert!(!control.invalid);
  let FormControlKind::Select(select) = &control.control else {
    panic!("expected select control kind");
  };
  assert!(!select.multiple);
  assert_eq!(select.size, 1);

  let idx = select.selected.first().copied().expect("selected option");
  let SelectItem::Option {
    label,
    value,
    selected,
    disabled,
    ..
  } = &select.items[idx]
  else {
    panic!("expected option item");
  };
  assert_eq!(label, "First");
  assert_eq!(value, "a");
  assert!(*selected);
  assert!(*disabled);
  assert_eq!(select_selected_value(select), Some("a"));
}

#[test]
fn required_select_with_disabled_selected_placeholder_remains_selected_and_invalid() {
  fn set_attr(node: &mut StyledNode, name: &str, value: &str) {
    match &mut node.node.node_type {
      DomNodeType::Element { attributes, .. } => {
        attributes.push((name.to_string(), value.to_string()));
      }
      _ => panic!("expected element node"),
    }
  }

  let placeholder_text = StyledNode {
    node_id: 0,
    subtree_size: 1,
    node: DomNode {
      node_type: DomNodeType::Text {
        content: "Choose".to_string(),
      },
      children: vec![],
    },
    styles: default_style(),
    starting_styles: StartingStyleSet::default(),
    before_styles: None,
    after_styles: None,
    marker_styles: None,
    first_line_styles: None,
    first_letter_styles: None,
    placeholder_styles: None,
    file_selector_button_styles: None,
    footnote_call_styles: None,
    footnote_marker_styles: None,
    slider_thumb_styles: None,
    slider_track_styles: None,
    progress_bar_styles: None,
    progress_value_styles: None,
    meter_bar_styles: None,
    meter_optimum_value_styles: None,
    meter_suboptimum_value_styles: None,
    meter_even_less_good_value_styles: None,
    assigned_slot: None,
    slotted_node_ids: Vec::new(),
    children: vec![],
  };

  let mut placeholder = styled_element("option");
  set_attr(&mut placeholder, "disabled", "");
  set_attr(&mut placeholder, "selected", "");
  set_attr(&mut placeholder, "value", "");
  placeholder.children.push(placeholder_text);

  let mut enabled = styled_element("option");
  set_attr(&mut enabled, "value", "x");

  let mut select = styled_element("select");
  set_attr(&mut select, "required", "");
  select.children = vec![placeholder, enabled];

  let control = create_form_control_replaced(&select, &[], None).expect("select form control");
  assert!(control.required);
  assert!(control.invalid);
  let FormControlKind::Select(select) = &control.control else {
    panic!("expected select control kind");
  };
  assert!(!select.multiple);
  assert_eq!(select.size, 1);

  let idx = select.selected.first().copied().expect("selected option");
  let SelectItem::Option {
    label,
    value,
    selected,
    disabled,
    ..
  } = &select.items[idx]
  else {
    panic!("expected option item");
  };
  assert_eq!(label, "Choose");
  assert_eq!(value, "");
  assert!(*selected);
  assert!(*disabled);
  assert_eq!(select_selected_value(select), Some(""));
}

#[test]
fn select_size_parsing_controls_effective_row_count() {
  fn set_attr(node: &mut StyledNode, name: &str, value: &str) {
    match &mut node.node.node_type {
      DomNodeType::Element { attributes, .. } => {
        attributes.push((name.to_string(), value.to_string()));
      }
      _ => panic!("expected element node"),
    }
  }

  let mut dropdown_size0 = styled_element("select");
  set_attr(&mut dropdown_size0, "size", "0");
  let control = create_form_control_replaced(&dropdown_size0, &[], None).expect("select form control");
  let FormControlKind::Select(select) = &control.control else {
    panic!("expected select control kind");
  };
  assert!(!select.multiple);
  assert_eq!(select.size, 1);

  let mut multi_default = styled_element("select");
  set_attr(&mut multi_default, "multiple", "");
  let control = create_form_control_replaced(&multi_default, &[], None).expect("select form control");
  let FormControlKind::Select(select) = &control.control else {
    panic!("expected select control kind");
  };
  assert!(select.multiple);
  assert_eq!(select.size, 4);

  let mut multi_invalid = styled_element("select");
  set_attr(&mut multi_invalid, "multiple", "");
  set_attr(&mut multi_invalid, "size", "abc");
  let control = create_form_control_replaced(&multi_invalid, &[], None).expect("select form control");
  let FormControlKind::Select(select) = &control.control else {
    panic!("expected select control kind");
  };
  assert!(select.multiple);
  assert_eq!(select.size, 4);

  let mut multi_size3 = styled_element("select");
  set_attr(&mut multi_size3, "multiple", "");
  set_attr(&mut multi_size3, "size", "3");
  let control = create_form_control_replaced(&multi_size3, &[], None).expect("select form control");
  let FormControlKind::Select(select) = &control.control else {
    panic!("expected select control kind");
  };
  assert!(select.multiple);
  assert_eq!(select.size, 3);
}

#[test]
fn pseudo_element_content_ignores_empty_url() {
  let styled = styled_element("div");
  let mut counters = CounterManager::new();
  let mut quote_depth = 0usize;

  let mut pseudo_style = ComputedStyle::default();
  pseudo_style.content_value = ContentValue::Items(vec![ContentItem::Url(
    crate::style::types::BackgroundImageUrl::new(String::new()),
  )]);
  let pseudo_style = Arc::new(pseudo_style);

  let pseudo_box =
    create_pseudo_element_box(&styled, &pseudo_style, None, "before", &mut counters, &mut quote_depth).expect(
      "pseudo-element boxes should still be generated when content isn't none/normal, even if the resolved url is empty",
    );
  assert!(
    pseudo_box.children.is_empty(),
    "empty url() content items should not generate replaced children"
  );
}

#[test]
fn pseudo_element_content_generates_box_for_empty_string() {
  let styled = styled_element("div");
  let mut counters = CounterManager::new();
  let mut quote_depth = 0usize;

  let mut pseudo_style = ComputedStyle::default();
  pseudo_style.content_value = ContentValue::Items(vec![ContentItem::String(String::new())]);
  let pseudo_style = Arc::new(pseudo_style);

  let pseudo_box = create_pseudo_element_box(
    &styled,
    &pseudo_style,
    None,
    "before",
    &mut counters,
    &mut quote_depth,
  )
  .expect("empty string content should still generate the pseudo-element box");
  assert!(
    pseudo_box.children.is_empty(),
    "empty string content shouldn't implicitly create placeholder text children"
  );
}

#[test]
fn marker_content_ignores_empty_url() {
  let styled = styled_element("li");
  let counters = CounterManager::new();
  let mut quote_depth = 0usize;

  let mut marker_style = ComputedStyle::default();
  marker_style.content_value = ContentValue::Items(vec![ContentItem::Url(
    crate::style::types::BackgroundImageUrl::new(String::new()),
  )]);

  assert!(
    marker_content_from_style(&styled, &marker_style, &counters, &mut quote_depth).is_none(),
    "empty url() content items should not generate marker images"
  );
}

fn contains_class(node: &BoxNode, class: &str) -> bool {
  if let Some(info) = &node.debug_info {
    if info.classes.iter().any(|c| c == class) {
      return true;
    }
  }
  node
    .children
    .iter()
    .any(|child| contains_class(child, class))
}

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
fn empty_ad_placeholders_are_kept_by_default() {
  use crate::css::parser::parse_stylesheet;
  use crate::style::cascade::apply_styles_with_media;
  use crate::style::media::MediaContext;

  for class in ["ad-height-hold", "ad__slot", "should-hold-space"] {
    let html = format!("<div class=\"{}\"></div>", class);
    let dom: DomNode = dom::parse_html(&html).unwrap();
    let stylesheet = parse_stylesheet("").unwrap();
    let styled = apply_styles_with_media(&dom, &stylesheet, &MediaContext::screen(800.0, 600.0));

    let box_tree = generate_box_tree_with_anonymous_fixup(&styled);
    assert!(
      contains_class(&box_tree.root, class),
      "default pipeline should not drop placeholder {class}"
    );
  }
}

#[test]
fn empty_ad_placeholders_are_dropped_with_site_compat() {
  use crate::css::parser::parse_stylesheet;
  use crate::style::cascade::apply_styles_with_media;
  use crate::style::media::MediaContext;

  let compat_options =
    BoxGenerationOptions::default().with_compat_profile(CompatProfile::SiteCompatibility);

  for class in ["ad-height-hold", "ad__slot", "should-hold-space"] {
    let html = format!("<div class=\"{}\"></div>", class);
    let dom: DomNode = dom::parse_html(&html).unwrap();
    let stylesheet = parse_stylesheet("").unwrap();
    let styled = apply_styles_with_media(&dom, &stylesheet, &MediaContext::screen(800.0, 600.0));

    let box_tree = super::generate_box_tree_with_anonymous_fixup_with_options(&styled, &compat_options)
      .expect("box tree");
    assert!(
      !contains_class(&box_tree.root, class),
      "compat mode should drop empty placeholder {class}"
    );
  }
}

#[test]
fn non_empty_ad_placeholders_are_kept_in_compat_mode() {
  use crate::css::parser::parse_stylesheet;
  use crate::style::cascade::apply_styles_with_media;
  use crate::style::media::MediaContext;

  let compat_options =
    BoxGenerationOptions::default().with_compat_profile(CompatProfile::SiteCompatibility);
  let dom: DomNode =
    dom::parse_html(r#"<div class="ad-height-hold"><span>ad content</span></div>"#).unwrap();
  let stylesheet = parse_stylesheet("").unwrap();
  let styled = apply_styles_with_media(&dom, &stylesheet, &MediaContext::screen(800.0, 600.0));

  let box_tree = super::generate_box_tree_with_anonymous_fixup_with_options(&styled, &compat_options)
    .expect("box tree");
  assert!(contains_class(&box_tree.root, "ad-height-hold"));
}

#[test]
fn hidden_onenav_overlay_is_retained_by_default() {
  use crate::css::parser::parse_stylesheet;
  use crate::style::cascade::apply_styles_with_media;
  use crate::style::media::MediaContext;

  let html = r#"
      <div>
          <div data-testid="one-nav-overlay" class="Overlay-ljtLmi"></div>
          <div class="FocusTrapContainer-jqtblI"><span class="content">keep me</span></div>
          <div class="keep">keep me too</div>
      </div>
  "#;
  let css = r#"
      [data-testid="one-nav-overlay"] {
          visibility: hidden;
          opacity: 0;
      }
  "#;

  let dom: DomNode = dom::parse_html(html).unwrap();
  let stylesheet = parse_stylesheet(css).unwrap();
  let styled = apply_styles_with_media(&dom, &stylesheet, &MediaContext::screen(800.0, 600.0));
  let box_tree = generate_box_tree_with_anonymous_fixup(&styled);

  assert!(contains_class(&box_tree.root, "keep"));
  assert!(contains_class(&box_tree.root, "content"));
  assert!(contains_class(&box_tree.root, "Overlay-ljtLmi"));
  assert!(contains_class(&box_tree.root, "FocusTrapContainer-jqtblI"));
}

#[test]
fn hidden_onenav_overlay_skips_drawer_with_site_compat() {
  use crate::css::parser::parse_stylesheet;
  use crate::style::cascade::apply_styles_with_media;
  use crate::style::media::MediaContext;

  let html = r#"
      <div>
          <div data-testid="one-nav-overlay" class="Overlay-ljtLmi"></div>
          <div class="FocusTrapContainer-jqtblI"><span class="content">keep me</span></div>
          <div class="keep">keep me too</div>
      </div>
  "#;
  let css = r#"
      [data-testid="one-nav-overlay"] {
          visibility: hidden;
          opacity: 0;
      }
  "#;

  let dom: DomNode = dom::parse_html(html).unwrap();
  let stylesheet = parse_stylesheet(css).unwrap();
  let styled = apply_styles_with_media(&dom, &stylesheet, &MediaContext::screen(800.0, 600.0));
  let compat_options =
    BoxGenerationOptions::default().with_compat_profile(CompatProfile::SiteCompatibility);
  let box_tree =
    super::generate_box_tree_with_anonymous_fixup_with_options(&styled, &compat_options)
      .expect("box tree");

  assert!(contains_class(&box_tree.root, "keep"));
  assert!(contains_class(&box_tree.root, "content"));
  assert!(!contains_class(&box_tree.root, "Overlay-ljtLmi"));
  assert!(!contains_class(&box_tree.root, "FocusTrapContainer-jqtblI"));
}

#[test]
fn visible_onenav_overlay_retained_with_drawer_in_compat_mode() {
  use crate::css::parser::parse_stylesheet;
  use crate::style::cascade::apply_styles_with_media;
  use crate::style::media::MediaContext;

  let html = r#"
      <div>
          <div data-testid="one-nav-overlay" class="Overlay-ljtLmi"></div>
          <div class="FocusTrapContainer-jqtblI"><span class="content">keep me</span></div>
      </div>
  "#;
  let css = r#"
      [data-testid="one-nav-overlay"] {
          visibility: visible;
          opacity: 1;
      }
  "#;

  let dom: DomNode = dom::parse_html(html).unwrap();
  let stylesheet = parse_stylesheet(css).unwrap();
  let styled = apply_styles_with_media(&dom, &stylesheet, &MediaContext::screen(800.0, 600.0));
  let compat_options =
    BoxGenerationOptions::default().with_compat_profile(CompatProfile::SiteCompatibility);
  let box_tree =
    super::generate_box_tree_with_anonymous_fixup_with_options(&styled, &compat_options)
      .expect("box tree");

  assert!(contains_class(&box_tree.root, "Overlay-ljtLmi"));
  assert!(contains_class(&box_tree.root, "FocusTrapContainer-jqtblI"));
  assert!(contains_class(&box_tree.root, "content"));
}

#[test]
fn flex_items_are_blockified() {
  use crate::css::types::StyleSheet;
  use crate::style::cascade::apply_styles;

  let html = r#"<div style="display:flex"><span class="item">Item</span></div>"#;
  let dom: dom::DomNode = dom::parse_html(html).expect("parse html");
  let styled = apply_styles(&dom, &StyleSheet::new());
  let box_tree = generate_box_tree_with_anonymous_fixup(&styled);

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
  use crate::css::types::StyleSheet;
  use crate::style::cascade::apply_styles;

  let html = r#"<div style="display:grid"><span class="item">Item</span></div>"#;
  let dom: dom::DomNode = dom::parse_html(html).expect("parse html");
  let styled = apply_styles(&dom, &StyleSheet::new());
  let box_tree = generate_box_tree_with_anonymous_fixup(&styled);

  let item = find_first_by_class(&box_tree.root, "item").expect("item present");

  assert_eq!(item.style.display, Display::Block);
  assert!(item.box_type.is_block_level());
}

#[test]
fn display_contents_descendants_are_blockified_as_items() {
  use crate::css::types::StyleSheet;
  use crate::style::cascade::apply_styles;

  let html = r#"<div style="display:flex"><div style="display:contents"><span class="item">Item</span></div></div>"#;
  let dom: dom::DomNode = dom::parse_html(html).expect("parse html");
  let styled = apply_styles(&dom, &StyleSheet::new());
  let box_tree = generate_box_tree_with_anonymous_fixup(&styled);

  let item = find_first_by_class(&box_tree.root, "item").expect("item present");

  assert_eq!(item.style.display, Display::Block);
  assert!(item.box_type.is_block_level());
}

#[test]
fn flex_replaced_items_are_blockified() {
  use crate::css::types::StyleSheet;
  use crate::style::cascade::apply_styles;

  let html = r#"<div style="display:flex"><img class="item" src="example.png"></div>"#;
  let dom: dom::DomNode = dom::parse_html(html).expect("parse html");
  let styled = apply_styles(&dom, &StyleSheet::new());
  let box_tree = generate_box_tree_with_anonymous_fixup(&styled);

  let item = find_first_by_class(&box_tree.root, "item").expect("item present");

  assert!(
    item.box_type.is_replaced(),
    "expected <img> to create a replaced box"
  );
  assert_eq!(
    item.style.display,
    Display::Block,
    "replaced flex/grid items should be blockified (used display becomes block-level)"
  );
}

#[test]
fn flex_container_ignores_collapsible_whitespace_text_nodes() {
  use crate::css::types::StyleSheet;
  use crate::style::cascade::apply_styles;

  let html = r#"
  <div class="flex" style="display:flex">
    <div class="a"></div>
    <div class="b"></div>
  </div>
"#;
  let dom: crate::dom::DomNode = dom::parse_html(html).expect("parse html");
  let styled = apply_styles(&dom, &StyleSheet::new());
  let box_tree = generate_box_tree_with_anonymous_fixup(&styled);

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
  use crate::css::types::StyleSheet;
  use crate::style::cascade::apply_styles;

  let html = r#"
  <div class="grid" style="display:grid">
    <div class="a"></div>
    <div class="b"></div>
  </div>
"#;
  let dom: crate::dom::DomNode = dom::parse_html(html).expect("parse html");
  let styled = apply_styles(&dom, &StyleSheet::new());
  let box_tree = generate_box_tree_with_anonymous_fixup(&styled);

  let grid = find_first_by_class(&box_tree.root, "grid").expect("grid node present");

  assert_eq!(
    grid.children.len(),
    2,
    "collapsible whitespace between grid items should not generate anonymous grid items"
  );
  assert!(node_has_class(&grid.children[0], "a"));
  assert!(node_has_class(&grid.children[1], "b"));
}

struct TestRenderDelayGuard;

impl TestRenderDelayGuard {
  fn set(ms: Option<u64>) -> Self {
    crate::render_control::set_test_render_delay_ms(ms);
    Self
  }
}

impl Drop for TestRenderDelayGuard {
  fn drop(&mut self) {
    crate::render_control::set_test_render_delay_ms(None);
  }
}

#[test]
fn box_generation_times_out_with_active_deadline() {
  use crate::error::{Error, RenderError, RenderStage};
  use crate::render_control::{DeadlineGuard, RenderDeadline};
  use crate::style::cascade::apply_styles;

  let _guard = TestRenderDelayGuard::set(Some(5));

  let mut repeated = String::new();
  for _ in 0..5000 {
    repeated.push_str("<div class=\"item\">content</div>");
  }
  let html = format!("<html><body>{repeated}</body></html>");

  let dom = dom::parse_html(&html).expect("parse html");
  let styled = apply_styles(&dom, &crate::css::types::StyleSheet::new());

  let deadline = RenderDeadline::new(Some(std::time::Duration::from_millis(1)), None);
  let _deadline_guard = DeadlineGuard::install(Some(&deadline));

  let err = super::generate_box_tree_with_anonymous_fixup(&styled).unwrap_err();
  match err {
    Error::Render(RenderError::Timeout { stage, .. }) => assert_eq!(stage, RenderStage::BoxTree),
    other => panic!("expected box_tree timeout, got {other:?}"),
  }
}

#[test]
fn container_query_rel_layout_uses_box_generation_options() {
  use crate::api::{FastRender, RenderArtifactRequest, RenderOptions};

  std::thread::Builder::new()
    .stack_size(8 * 1024 * 1024)
    .spawn(|| {
      // Force a container-query second pass (fingerprints differ) while running under the
      // site-compatibility profile. The second-pass box generation must keep using the
      // compatibility options; otherwise empty ad placeholders would reappear.
      let html = r#"
      <style>
        .container {
          container-type: inline-size;
          width: 200px;
        }
        .target { display: none; }
        @container (min-width: 150px) {
          .target { display: block; }
        }
      </style>
      <div class="container">
        <div class="ad-height-hold"></div>
        <div class="target">hello</div>
      </div>
    "#;

      let mut renderer = FastRender::builder()
        .viewport_size(800, 600)
        .with_site_compat_hacks()
        .build()
        .expect("build renderer");

      let report = renderer
        .render_html_with_stylesheets_report(
          html,
          "https://example.com",
          RenderOptions::default(),
          RenderArtifactRequest {
            box_tree: true,
            ..RenderArtifactRequest::default()
          },
        )
        .expect("render html");

      let box_tree = report
        .artifacts
        .box_tree
        .expect("expected box tree artifact");
      assert!(
        !contains_class(&box_tree.root, "ad-height-hold"),
        "empty ad placeholders should remain dropped after container-query relayout",
      );
    })
    .expect("spawn test thread")
    .join()
    .expect("join test thread");
}

fn find_styled_node_id_by_element_id(styled: &crate::style::cascade::StyledNode, id: &str) -> Option<usize> {
  if styled.node.get_attribute_ref("id") == Some(id) {
    return Some(styled.node_id);
  }
  styled
    .children
    .iter()
    .find_map(|child| find_styled_node_id_by_element_id(child, id))
}

fn find_box_by_styled_id<'a>(node: &'a BoxNode, styled_id: usize) -> Option<&'a BoxNode> {
  if node.styled_node_id == Some(styled_id) {
    return Some(node);
  }
  node
    .children
    .iter()
    .find_map(|child| find_box_by_styled_id(child, styled_id))
}

fn has_descendant_with_styled_id(node: &BoxNode, styled_id: usize) -> bool {
  if node.styled_node_id == Some(styled_id) {
    return true;
  }
  node
    .children
    .iter()
    .any(|child| has_descendant_with_styled_id(child, styled_id))
}

fn count_form_control_replacements(node: &BoxNode) -> usize {
  let mut count = 0usize;
  if let BoxType::Replaced(repl) = &node.box_type {
    if matches!(repl.replaced_type, ReplacedType::FormControl(_)) {
      count += 1;
    }
  }
  for child in node.children.iter() {
    count += count_form_control_replacements(child);
  }
  count
}

fn has_generated_pseudo(node: &BoxNode, pseudo: GeneratedPseudoElement) -> bool {
  if node.generated_pseudo == Some(pseudo) {
    return true;
  }
  node
    .children
    .iter()
    .any(|child| has_generated_pseudo(child, pseudo))
}

#[test]
fn button_appearance_none_preserves_dom_children() {
  use crate::css::types::StyleSheet;
  use crate::style::cascade::apply_styles;

  let html = "<html><body><button id=\"btn\" style=\"appearance:none\"><span id=\"inner\">Hello</span></button></body></html>";
  let dom = dom::parse_html(html).expect("parse html");
  let styled = apply_styles(&dom, &StyleSheet::new());

  let btn_id = find_styled_node_id_by_element_id(&styled, "btn").expect("button styled node id");
  let span_id = find_styled_node_id_by_element_id(&styled, "inner").expect("span styled node id");

  let tree = generate_box_tree(&styled);
  assert_eq!(
    count_form_control_replacements(&tree.root),
    0,
    "appearance:none buttons should not create replaced form controls"
  );

  let btn_box = find_box_by_styled_id(&tree.root, btn_id).expect("button box");
  assert!(
    has_descendant_with_styled_id(btn_box, span_id),
    "expected button descendants to generate boxes when appearance:none"
  );
  assert!(
    btn_box.text().is_none(),
    "button box should not collapse children into a synthetic label text node"
  );
}

#[test]
fn range_appearance_none_generates_slider_track_and_thumb_boxes() {
  use crate::css::types::StyleSheet;
  use crate::style::cascade::apply_styles;

  let html =
    "<html><body><input id=\"slider\" type=\"range\" style=\"appearance:none\" /></body></html>";
  let dom = dom::parse_html(html).expect("parse html");
  let styled = apply_styles(&dom, &StyleSheet::new());

  let slider_id =
    find_styled_node_id_by_element_id(&styled, "slider").expect("slider styled node id");
  let tree = generate_box_tree(&styled);

  assert_eq!(
    count_form_control_replacements(&tree.root),
    0,
    "appearance:none range inputs should not create replaced form controls"
  );

  let slider_box = find_box_by_styled_id(&tree.root, slider_id).expect("slider box");
  assert!(
    has_generated_pseudo(slider_box, GeneratedPseudoElement::SliderTrack),
    "expected range track pseudo-element box to be generated"
  );
  assert!(
    has_generated_pseudo(slider_box, GeneratedPseudoElement::SliderThumb),
    "expected range thumb pseudo-element box to be generated"
  );
}

#[test]
fn file_input_appearance_none_generates_file_selector_button_box() {
  use crate::css::types::StyleSheet;
  use crate::style::cascade::apply_styles;

  let html =
    "<html><body><input id=\"file\" type=\"file\" style=\"appearance:none\" /></body></html>";
  let dom = dom::parse_html(html).expect("parse html");
  let styled = apply_styles(&dom, &StyleSheet::new());

  let file_id = find_styled_node_id_by_element_id(&styled, "file").expect("file styled node id");
  let tree = generate_box_tree(&styled);

  assert_eq!(
    count_form_control_replacements(&tree.root),
    0,
    "appearance:none file inputs should not create replaced form controls"
  );

  let file_box = find_box_by_styled_id(&tree.root, file_id).expect("file input box");
  assert!(
    has_generated_pseudo(file_box, GeneratedPseudoElement::FileSelectorButton),
    "expected file-selector-button pseudo-element box to be generated"
  );
}

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

fn collect_option_dom_ids(select: &DomNode, ids: &HashMap<*const DomNode, usize>) -> Vec<usize> {
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
  use crate::css::types::StyleSheet;
  use crate::style::cascade::apply_styles;

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
  let expected_option_ids = collect_option_dom_ids(select_node, &dom_ids);

  let styled = apply_styles(&dom, &StyleSheet::new());
  let box_tree = generate_box_tree(&styled);

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

fn contains_tag(node: &BoxNode, tag: &str) -> bool {
  if let Some(info) = &node.debug_info {
    if info.tag_name.as_deref() == Some(tag) {
      return true;
    }
  }

  node.children.iter().any(|child| contains_tag(child, tag))
}

fn collect_replaced_tag_names(node: &BoxNode, out: &mut Vec<String>) {
  if let BoxType::Replaced(_) = &node.box_type {
    if let Some(tag) = node.debug_info.as_ref().and_then(|info| info.tag_name.clone()) {
      out.push(tag);
    }
  }

  for child in node.children.iter() {
    collect_replaced_tag_names(child, out);
  }
}

#[test]
fn option_like_elements_outside_select_do_not_generate_boxes() {
  use crate::css::types::StyleSheet;
  use crate::style::cascade::apply_styles;

  let html = "<html><body><option id=\"orphan\">Loose</option><optgroup label=\"g\"><option>One</option></optgroup></body></html>";
  let dom = dom::parse_html(html).expect("parse html");
  let styled = apply_styles(&dom, &StyleSheet::new());
  let box_tree = generate_box_tree(&styled);

  assert!(contains_tag(&box_tree.root, "html"));
  assert!(!contains_tag(&box_tree.root, "option"));
  assert!(!contains_tag(&box_tree.root, "optgroup"));
}

#[test]
fn select_generates_single_replaced_box_without_option_children() {
  use crate::css::types::StyleSheet;
  use crate::style::cascade::apply_styles;

  let html = "<html><body><select id=\"flavors\"><option>Vanilla</option><optgroup label=\"sweet\"><option selected>Chocolate</option></optgroup></select></body></html>";
  let dom = dom::parse_html(html).expect("parse html");
  let styled = apply_styles(&dom, &StyleSheet::new());
  let box_tree = generate_box_tree(&styled);

  assert!(contains_tag(&box_tree.root, "select"));
  assert!(!contains_tag(&box_tree.root, "option"));
  assert!(!contains_tag(&box_tree.root, "optgroup"));

  let mut replaced_tags = Vec::new();
  collect_replaced_tag_names(&box_tree.root, &mut replaced_tags);
  assert_eq!(replaced_tags, vec!["select".to_string()]);
}

fn find_inline_svg(node: &BoxNode) -> Option<&ReplacedBox> {
  if let BoxType::Replaced(replaced) = &node.box_type {
    if matches!(replaced.replaced_type, ReplacedType::Svg { .. }) {
      return Some(replaced);
    }
  }
  for child in node.children.iter() {
    if let Some(found) = find_inline_svg(child) {
      return Some(found);
    }
  }
  None
}

fn svg_replaced_box(svg_markup: &str) -> ReplacedBox {
  svg_replaced_box_with_stylesheet(svg_markup, &crate::css::types::StyleSheet::new())
}

fn svg_replaced_box_with_stylesheet(svg_markup: &str, stylesheet: &crate::css::types::StyleSheet) -> ReplacedBox {
  let html = format!("<html><body>{}</body></html>", svg_markup);
  let dom = dom::parse_html(&html).expect("parse html");
  let styled = crate::style::cascade::apply_styles(&dom, stylesheet);
  let tree = generate_box_tree(&styled);
  find_inline_svg(&tree.root)
    .cloned()
    .expect("inline svg replaced box")
}

#[test]
fn svg_numeric_width_height_used_as_intrinsic_size() {
  let replaced = svg_replaced_box(r#"<svg width="200" height="100"></svg>"#);
  assert_eq!(replaced.intrinsic_size, Some(Size::new(200.0, 100.0)));
}

#[test]
fn svg_parses_absolute_length_units() {
  let replaced = svg_replaced_box(r#"<svg width="2in" height="1in"></svg>"#);
  assert_eq!(replaced.intrinsic_size, Some(Size::new(192.0, 96.0)));
}

#[test]
fn svg_percentage_lengths_fall_back_to_default_intrinsic_size() {
  let replaced = svg_replaced_box(r#"<svg width="100%" height="100%"></svg>"#);
  assert_eq!(replaced.intrinsic_size, Some(Size::new(300.0, 150.0)));
  assert_eq!(replaced.aspect_ratio, None);
}

#[test]
fn svg_viewbox_sets_aspect_ratio_with_default_dimensions() {
  let replaced = svg_replaced_box(r#"<svg viewBox="0 0 40 20"></svg>"#);
  assert_eq!(replaced.intrinsic_size, Some(Size::new(300.0, 150.0)));
  assert_eq!(replaced.aspect_ratio, Some(2.0));
}

#[test]
fn svg_em_units_resolve_against_default_font_size() {
  let replaced = svg_replaced_box(r#"<svg width="1em" height="2em"></svg>"#);
  let size = replaced.intrinsic_size.expect("intrinsic size");
  assert!((size.width - 16.0).abs() < 0.01);
  assert!((size.height - 32.0).abs() < 0.01);
}

#[test]
fn svg_em_units_resolve_against_css_font_size() {
  use crate::css::parser::parse_stylesheet;

  let stylesheet = parse_stylesheet("svg{font-size:20px}").expect("parse css");
  let replaced =
    svg_replaced_box_with_stylesheet(r#"<svg width="1em" height="1em"></svg>"#, &stylesheet);
  let size = replaced.intrinsic_size.expect("intrinsic size");
  assert!((size.width - 20.0).abs() < 0.01);
  assert!((size.height - 20.0).abs() < 0.01);
}

fn find_inline_svg_box(node: &BoxNode) -> Option<&BoxNode> {
  if let BoxType::Replaced(replaced) = &node.box_type {
    if matches!(replaced.replaced_type, ReplacedType::Svg { .. }) {
      return Some(node);
    }
  }
  for child in &node.children {
    if let Some(found) = find_inline_svg_box(child) {
      return Some(found);
    }
  }
  None
}

#[test]
fn svg_root_transform_attribute_is_neutralized_in_serialized_markup() {
  use crate::css::types::StyleSheet;
  use crate::style::cascade;

  let html = r#"
  <html>
    <body>
      <svg transform="translate(10 0)" width="10" height="10">
        <rect width="10" height="10" />
      </svg>
    </body>
  </html>
"#;

  let dom = dom::parse_html(html).expect("parse html");
  let styled = cascade::apply_styles(&dom, &StyleSheet::new());
  let tree = generate_box_tree(&styled);

  let svg_box = find_inline_svg_box(&tree.root).expect("inline svg box");
  assert!(
    svg_box.style.has_transform(),
    "svg transform attribute should participate in cascade and be applied externally"
  );

  let BoxType::Replaced(replaced) = &svg_box.box_type else {
    panic!("expected replaced box");
  };
  let ReplacedType::Svg { content } = &replaced.replaced_type else {
    panic!("expected svg replaced type");
  };

  let doc = roxmltree::Document::parse(&content.svg).expect("parse serialized svg");
  let root = doc.root_element();
  assert_eq!(root.tag_name().name(), "svg");
  assert!(
    root.attribute("transform").is_none(),
    "serialized root svg must not include transform attribute"
  );
  let style = root.attribute("style").expect("root style attribute");
  assert!(
    style.contains("transform: none"),
    "serialized root svg must neutralize transform via style attribute: {style}"
  );
}

fn find_inline_svg_content(node: &BoxNode) -> Option<&SvgContent> {
  if let BoxType::Replaced(replaced) = &node.box_type {
    if let ReplacedType::Svg { content } = &replaced.replaced_type {
      return Some(content);
    }
  }
  for child in node.children.iter() {
    if let Some(found) = find_inline_svg_content(child) {
      return Some(found);
    }
  }
  None
}

fn serialized_inline_svg_with_stylesheet(
  svg_markup: &str,
  stylesheet: &crate::css::types::StyleSheet,
) -> String {
  let html = format!("<html><body>{}</body></html>", svg_markup);
  let dom = dom::parse_html(&html).expect("parse html");
  let styled = crate::style::cascade::apply_styles(&dom, stylesheet);
  let tree = generate_box_tree(&styled);
  find_inline_svg_content(&tree.root)
    .expect("inline svg replaced box")
    .svg
    .clone()
}

#[test]
fn css_transform_overrides_svg_transform_attribute_in_serialized_svg() {
  use crate::css::parser::parse_stylesheet;

  let svg_markup = r#"
  <svg>
  <g id="g" transform="translate(200 0)">
      <rect width="10" height="10"></rect>
    </g>
  </svg>
  "#;
  let stylesheet = parse_stylesheet("g { transform: translate(100px, 0px); }").unwrap();
  let serialized = serialized_inline_svg_with_stylesheet(svg_markup, &stylesheet);
  let doc = roxmltree::Document::parse(&serialized).expect("parse serialized svg");
  let g = doc
    .descendants()
    .find(|node| node.is_element() && node.attribute("id") == Some("g"))
    .expect("g element");
  assert_eq!(g.attribute("transform"), Some("translate(100 0)"));
}

#[test]
fn css_transform_none_removes_svg_transform_attribute_in_serialized_svg() {
  use crate::css::parser::parse_stylesheet;

  let svg_markup = r#"
  <svg>
  <g id="g" transform="translate(200 0)">
      <rect width="10" height="10"></rect>
    </g>
  </svg>
  "#;
  let stylesheet = parse_stylesheet("g { transform: none; }").unwrap();
  let serialized = serialized_inline_svg_with_stylesheet(svg_markup, &stylesheet);
  let doc = roxmltree::Document::parse(&serialized).expect("parse serialized svg");
  let g = doc
    .descendants()
    .find(|node| node.is_element() && node.attribute("id") == Some("g"))
    .expect("g element");
  assert!(g.attribute("transform").is_none());
}

fn serialized_inline_svg_from_html(html: &str) -> String {
  use crate::css::parser::extract_css;
  use crate::style::cascade;

  let html = format!("<html><body>{}</body></html>", html);
  let dom = dom::parse_html(&html).expect("parse html");
  let stylesheet = extract_css(&dom).expect("extract css");
  let styled = cascade::apply_styles(&dom, &stylesheet);
  let tree = generate_box_tree(&styled);
  find_inline_svg_content(&tree.root)
    .expect("inline svg replaced box")
    .svg
    .clone()
}

#[test]
fn svg_serialization_overrides_pattern_transform_with_pattern_transform_attribute() {
  let svg = serialized_inline_svg_from_html(
    r#"
  <style>pattern{transform:translate(100px,0px)}</style>
  <svg width="10" height="10">
    <defs>
      <pattern id="p" patternTransform="translate(200 0)" width="10" height="10" patternUnits="userSpaceOnUse">
        <rect width="10" height="10" fill="red" />
      </pattern>
    </defs>
    <rect width="10" height="10" fill="url(#p)" />
  </svg>
  "#,
  );

  let doc = roxmltree::Document::parse(&svg).expect("parse serialized svg");
  let pattern = doc
    .descendants()
    .find(|node| node.is_element() && node.tag_name().name().eq_ignore_ascii_case("pattern"))
    .expect("pattern element");

  assert_eq!(
    pattern.attribute("patternTransform"),
    Some("translate(100 0)"),
    "expected CSS transform to override patternTransform during serialization"
  );
  assert!(
    pattern.attribute("transform").is_none(),
    "pattern elements must not receive a transform= attribute"
  );
}

#[test]
fn svg_serialization_transform_none_removes_pattern_transform_attribute() {
  let svg = serialized_inline_svg_from_html(
    r#"
  <style>pattern{transform:none}</style>
  <svg width="10" height="10">
    <defs>
      <pattern id="p" patternTransform="translate(200 0)" width="10" height="10" patternUnits="userSpaceOnUse">
        <rect width="10" height="10" fill="red" />
      </pattern>
    </defs>
    <rect width="10" height="10" fill="url(#p)" />
  </svg>
  "#,
  );

  let doc = roxmltree::Document::parse(&svg).expect("parse serialized svg");
  let pattern = doc
    .descendants()
    .find(|node| node.is_element() && node.tag_name().name().eq_ignore_ascii_case("pattern"))
    .expect("pattern element");

  assert!(
    pattern.attribute("patternTransform").is_none(),
    "expected transform:none to cancel patternTransform during serialization"
  );
  assert!(pattern.attribute("transform").is_none());
}

#[test]
fn svg_serialization_overrides_gradient_transform_with_gradient_transform_attribute() {
  let svg = serialized_inline_svg_from_html(
    r#"
  <style>linearGradient{transform:translate(100px,0px)}</style>
  <svg width="10" height="10">
    <defs>
      <linearGradient id="g" gradientTransform="translate(200 0)">
        <stop offset="0" stop-color="red" />
        <stop offset="1" stop-color="blue" />
      </linearGradient>
    </defs>
    <rect width="10" height="10" fill="url(#g)" />
  </svg>
  "#,
  );

  let doc = roxmltree::Document::parse(&svg).expect("parse serialized svg");
  let gradient = doc
    .descendants()
    .find(|node| {
      node.is_element()
        && node
          .tag_name()
          .name()
          .eq_ignore_ascii_case("linearGradient")
    })
    .expect("linearGradient element");

  assert_eq!(
    gradient.attribute("gradientTransform"),
    Some("translate(100 0)"),
    "expected CSS transform to override gradientTransform during serialization"
  );
  assert!(
    gradient.attribute("transform").is_none(),
    "gradient elements must not receive a transform= attribute"
  );
}

#[test]
fn svg_serialization_transform_none_removes_gradient_transform_attribute() {
  let svg = serialized_inline_svg_from_html(
    r#"
  <style>linearGradient{transform:none}</style>
  <svg width="10" height="10">
    <defs>
      <linearGradient id="g" gradientTransform="translate(200 0)">
        <stop offset="0" stop-color="red" />
        <stop offset="1" stop-color="blue" />
      </linearGradient>
    </defs>
    <rect width="10" height="10" fill="url(#g)" />
  </svg>
  "#,
  );

  let doc = roxmltree::Document::parse(&svg).expect("parse serialized svg");
  let gradient = doc
    .descendants()
    .find(|node| {
      node.is_element()
        && node
          .tag_name()
          .name()
          .eq_ignore_ascii_case("linearGradient")
    })
    .expect("linearGradient element");

  assert!(
    gradient.attribute("gradientTransform").is_none(),
    "expected transform:none to cancel gradientTransform during serialization"
  );
  assert!(gradient.attribute("transform").is_none());
}

fn g_style_from_serialized_svg(svg: &str) -> Option<(Option<String>, Option<String>)> {
  let doc = roxmltree::Document::parse(svg).ok()?;
  let g = doc
    .descendants()
    .find(|node| node.has_tag_name("g") && node.attribute("id").is_some_and(|id| id == "g"))?;
  Some((
    g.attribute("transform").map(|v| v.to_string()),
    g.attribute("style").map(|v| v.to_string()),
  ))
}

#[test]
fn svg_transform_percentage_falls_back_to_css_style_text() {
  use crate::css::parser::parse_stylesheet;
  use crate::style::cascade;

  let html = r#"
  <html>
    <body>
      <svg>
        <g id="g">
          <rect id="r" x="0" y="0" width="10" height="10"></rect>
        </g>
      </svg>
    </body>
  </html>
"#;
  let stylesheet = parse_stylesheet("g{ transform: translateX(100%); }").expect("parse css");
  let dom = dom::parse_html(html).expect("parse html");
  let styled = cascade::apply_styles(&dom, &stylesheet);
  let tree = generate_box_tree(&styled);
  let svg = find_inline_svg_content(&tree.root)
    .expect("inline svg")
    .svg
    .as_str();

  let (transform_attr, style_attr) =
    g_style_from_serialized_svg(svg).expect("find g in serialized svg");
  assert!(
    transform_attr.is_none(),
    "expected percentage transforms to be serialized as CSS text, not an SVG transform attribute"
  );
  let style_attr = style_attr.expect("style attribute");
  assert!(
    style_attr.contains("transform:"),
    "expected serialized g style to include transform declaration: {style_attr:?}"
  );
  assert!(
    style_attr.contains("100%"),
    "expected serialized g style to preserve percent length: {style_attr:?}"
  );
}

#[test]
fn svg_transform_calc_falls_back_to_css_style_text() {
  use crate::css::parser::parse_stylesheet;
  use crate::style::cascade;

  let html = r#"
  <html>
    <body>
      <svg>
        <g id="g">
          <rect id="r" x="0" y="0" width="10" height="10"></rect>
        </g>
      </svg>
    </body>
  </html>
"#;
  let stylesheet =
    parse_stylesheet("g{ transform: translateX(calc(100% + 2px)); }").expect("parse css");
  let dom = dom::parse_html(html).expect("parse html");
  let styled = cascade::apply_styles(&dom, &stylesheet);
  let tree = generate_box_tree(&styled);
  let svg = find_inline_svg_content(&tree.root)
    .expect("inline svg")
    .svg
    .as_str();

  let (transform_attr, style_attr) =
    g_style_from_serialized_svg(svg).expect("find g in serialized svg");
  assert!(
    transform_attr.is_none(),
    "expected calc transforms to be serialized as CSS text, not an SVG transform attribute"
  );
  let style_attr = style_attr.expect("style attribute");
  assert!(
    style_attr.contains("transform:"),
    "expected serialized g style to include transform declaration: {style_attr:?}"
  );
  assert!(
    style_attr.contains("calc("),
    "expected serialized g style to preserve calc() text: {style_attr:?}"
  );
  assert!(
    style_attr.contains("100%"),
    "expected serialized g style to preserve percentage inside calc(): {style_attr:?}"
  );
}

#[test]
fn svg_transform_unserializable_keeps_authored_transform_attribute() {
  use crate::css::parser::parse_stylesheet;
  use crate::style::cascade;

  let html = r#"
  <html>
    <body>
      <svg>
        <g id="g" transform="translate(10 0)">
          <rect id="r" x="0" y="0" width="10" height="10"></rect>
        </g>
      </svg>
    </body>
  </html>
"#;
  let stylesheet = parse_stylesheet("g{ transform: translateX(100%); }").expect("parse css");
  let dom = dom::parse_html(html).expect("parse html");
  let styled = cascade::apply_styles(&dom, &stylesheet);
  let tree = generate_box_tree(&styled);
  let svg = find_inline_svg_content(&tree.root)
    .expect("inline svg")
    .svg
    .as_str();

  let (transform_attr, style_attr) =
    g_style_from_serialized_svg(svg).expect("find g in serialized svg");
  let transform_attr = transform_attr.expect("authored transform attribute");
  assert!(
    transform_attr.contains("translate(10"),
    "expected authored transform attribute to be preserved: {transform_attr:?}"
  );

  let style_attr = style_attr.expect("style attribute");
  assert!(
    style_attr.contains("transform:"),
    "expected serialized g style to include transform declaration: {style_attr:?}"
  );
  assert!(
    style_attr.contains("100%"),
    "expected serialized g style to preserve percent length: {style_attr:?}"
  );
}
