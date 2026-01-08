use fastrender::css::parser::parse_stylesheet;
use fastrender::dom;
use fastrender::style::cascade::apply_styles_with_media;
use fastrender::style::cascade::StyledNode;
use fastrender::style::media::MediaContext;

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

  let node = find_first(&styled, &mut |n| n.node.get_attribute_ref("popover").is_some())
    .expect("popover node");
  assert_eq!(node.styles.display.to_string(), "none");
}

#[test]
fn open_popover_respects_author_display() {
  let dom = dom::parse_html(r#"<tool-tip popover="manual" open>hello</tool-tip>"#).unwrap();
  let stylesheet = parse_stylesheet("tool-tip { display: block; }").unwrap();
  let styled = apply_styles_with_media(&dom, &stylesheet, &MediaContext::screen(800.0, 600.0));

  let node = find_first(&styled, &mut |n| n.node.get_attribute_ref("popover").is_some())
    .expect("popover node");
  assert_eq!(node.styles.display.to_string(), "block");
}

#[test]
fn closed_dialog_forces_display_none_even_when_author_sets_display() {
  let dom = dom::parse_html(r#"<dialog>hello</dialog>"#).unwrap();
  let stylesheet = parse_stylesheet("dialog { display: block; }").unwrap();
  let styled = apply_styles_with_media(&dom, &stylesheet, &MediaContext::screen(800.0, 600.0));

  let node = find_first(
    &styled,
    &mut |n| n.node.tag_name().is_some_and(|tag| tag.eq_ignore_ascii_case("dialog")),
  )
  .expect("dialog node");
  assert_eq!(node.styles.display.to_string(), "none");
}

#[test]
fn open_dialog_respects_author_display() {
  let dom = dom::parse_html(r#"<dialog open>hello</dialog>"#).unwrap();
  let stylesheet = parse_stylesheet("dialog { display: block; }").unwrap();
  let styled = apply_styles_with_media(&dom, &stylesheet, &MediaContext::screen(800.0, 600.0));

  let node = find_first(
    &styled,
    &mut |n| n.node.tag_name().is_some_and(|tag| tag.eq_ignore_ascii_case("dialog")),
  )
  .expect("dialog node");
  assert_eq!(node.styles.display.to_string(), "block");
}
