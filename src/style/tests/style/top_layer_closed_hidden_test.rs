use crate::css::parser::parse_stylesheet;
use crate::dom;
use crate::style::cascade::apply_styles_with_media;
use crate::style::cascade::StyledNode;
use crate::style::media::MediaContext;

fn find_first<'a, F: FnMut(&'a StyledNode) -> bool>(
  node: &'a StyledNode,
  pred: &mut F,
) -> Option<&'a StyledNode> {
  if pred(node) {
    return Some(node);
  }
  for child in node.children.iter() {
    if let Some(found) = find_first(child, pred) {
      return Some(found);
    }
  }
  None
}

#[test]
fn closed_popover_forces_display_none_even_when_author_sets_display() {
  let dom = dom::parse_html(r#"<tool-tip popover="manual">hello</tool-tip>"#).unwrap();
  let stylesheet = parse_stylesheet("tool-tip { display: block; }").unwrap();
  let styled = apply_styles_with_media(&dom, &stylesheet, &MediaContext::screen(800.0, 600.0));

  let node = find_first(&styled, &mut |n| {
    n.node.get_attribute_ref("popover").is_some()
  })
  .expect("popover node");
  assert_eq!(node.styles.display.to_string(), "none");
}

#[test]
fn open_popover_respects_author_display() {
  let dom = dom::parse_html(r#"<tool-tip popover="manual" open>hello</tool-tip>"#).unwrap();
  let stylesheet = parse_stylesheet("tool-tip { display: block; }").unwrap();
  let styled = apply_styles_with_media(&dom, &stylesheet, &MediaContext::screen(800.0, 600.0));

  let node = find_first(&styled, &mut |n| {
    n.node.get_attribute_ref("popover").is_some()
  })
  .expect("popover node");
  assert_eq!(node.styles.display.to_string(), "block");
}

#[test]
fn closed_dialog_forces_display_none_even_when_author_sets_display() {
  let dom = dom::parse_html(r#"<dialog>hello</dialog>"#).unwrap();
  let stylesheet = parse_stylesheet("dialog { display: block; }").unwrap();
  let styled = apply_styles_with_media(&dom, &stylesheet, &MediaContext::screen(800.0, 600.0));

  let node = find_first(&styled, &mut |n| {
    n.node
      .tag_name()
      .is_some_and(|tag| tag.eq_ignore_ascii_case("dialog"))
  })
  .expect("dialog node");
  assert_eq!(node.styles.display.to_string(), "none");
}

#[test]
fn open_dialog_respects_author_display() {
  let dom = dom::parse_html(r#"<dialog open>hello</dialog>"#).unwrap();
  let stylesheet = parse_stylesheet("dialog { display: block; }").unwrap();
  let styled = apply_styles_with_media(&dom, &stylesheet, &MediaContext::screen(800.0, 600.0));

  let node = find_first(&styled, &mut |n| {
    n.node
      .tag_name()
      .is_some_and(|tag| tag.eq_ignore_ascii_case("dialog"))
  })
  .expect("dialog node");
  assert_eq!(node.styles.display.to_string(), "block");
}

#[test]
fn svg_dialog_does_not_use_html_top_layer_semantics() {
  // `dialog` is an HTML element. An SVG element named `dialog` must not be forced `display: none`
  // when closed, nor treated as being in the top layer when `open` is present.
  let dom = dom::parse_html(
    r#"<svg>
      <dialog id="dlg-open" open>open</dialog>
      <dialog id="dlg-closed">closed</dialog>
    </svg>"#,
  )
  .unwrap();
  let stylesheet = parse_stylesheet("#dlg-open, #dlg-closed { display: block; }").unwrap();
  let styled = apply_styles_with_media(&dom, &stylesheet, &MediaContext::screen(800.0, 600.0));

  let open = find_first(&styled, &mut |n| n.node.get_attribute_ref("id") == Some("dlg-open"))
    .expect("dlg-open node");
  assert_eq!(open.styles.display.to_string(), "block");
  assert!(
    open.styles.top_layer.is_none(),
    "SVG <dialog open> must not become top-layer content"
  );

  let closed = find_first(&styled, &mut |n| n.node.get_attribute_ref("id") == Some("dlg-closed"))
    .expect("dlg-closed node");
  assert_eq!(closed.styles.display.to_string(), "block");
  assert!(
    closed.styles.top_layer.is_none(),
    "SVG <dialog> must not become top-layer content"
  );
}

#[test]
fn svg_popover_does_not_use_html_top_layer_semantics() {
  // Popover semantics are HTML-only; an SVG element with a `popover` attribute should not be
  // hidden when closed nor promoted to the top layer when `open` is present.
  let dom = dom::parse_html(
    r#"<svg>
      <g id="pop-open" popover="manual" open>open</g>
      <g id="pop-closed" popover="manual">closed</g>
    </svg>"#,
  )
  .unwrap();
  let stylesheet = parse_stylesheet("#pop-open, #pop-closed { display: block; }").unwrap();
  let styled = apply_styles_with_media(&dom, &stylesheet, &MediaContext::screen(800.0, 600.0));

  let open = find_first(&styled, &mut |n| n.node.get_attribute_ref("id") == Some("pop-open"))
    .expect("pop-open node");
  assert_eq!(open.styles.display.to_string(), "block");
  assert!(
    open.styles.top_layer.is_none(),
    "SVG [popover][open] must not become top-layer content"
  );

  let closed = find_first(&styled, &mut |n| n.node.get_attribute_ref("id") == Some("pop-closed"))
    .expect("pop-closed node");
  assert_eq!(closed.styles.display.to_string(), "block");
  assert!(
    closed.styles.top_layer.is_none(),
    "SVG [popover] must not become top-layer content"
  );
}
