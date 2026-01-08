use fastrender::css::parser::parse_stylesheet;
use fastrender::dom;
use fastrender::style::cascade::apply_styles_with_media;
use fastrender::style::cascade::StyledNode;
use fastrender::style::media::MediaContext;
use fastrender::style::types::{
  TextDecorationSkipBox, TextDecorationSkipInk, TextDecorationSkipSelf, TextDecorationSkipSpaces,
};

fn find_first<'a>(node: &'a StyledNode, tag: &str) -> Option<&'a StyledNode> {
  if let Some(name) = node.node.tag_name() {
    if name.eq_ignore_ascii_case(tag) {
      return Some(node);
    }
  }
  for child in node.children.iter() {
    if let Some(found) = find_first(child, tag) {
      return Some(found);
    }
  }
  None
}

#[test]
fn text_decoration_skip_none_sets_subproperties_and_inheritance() {
  let dom =
    dom::parse_html(r#"<div style="text-decoration-skip: none"><span>child</span></div>"#).unwrap();
  let stylesheet = parse_stylesheet("").unwrap();
  let styled = apply_styles_with_media(&dom, &stylesheet, &MediaContext::screen(800.0, 600.0));

  let parent = find_first(&styled, "div").expect("div");
  let child = find_first(parent, "span").expect("span");

  assert_eq!(parent.styles.text_decoration_skip_self, TextDecorationSkipSelf::NoSkip);
  assert_eq!(parent.styles.text_decoration_skip_box, TextDecorationSkipBox::None);
  assert_eq!(
    parent.styles.text_decoration_skip_spaces,
    TextDecorationSkipSpaces::None
  );
  assert_eq!(parent.styles.text_decoration_skip_ink, TextDecorationSkipInk::None);

  // `text-decoration-skip-self` does not inherit, but the other subproperties do.
  assert_eq!(child.styles.text_decoration_skip_self, TextDecorationSkipSelf::Auto);
  assert_eq!(child.styles.text_decoration_skip_box, TextDecorationSkipBox::None);
  assert_eq!(
    child.styles.text_decoration_skip_spaces,
    TextDecorationSkipSpaces::None
  );
  assert_eq!(child.styles.text_decoration_skip_ink, TextDecorationSkipInk::None);
}

#[test]
fn text_decoration_skip_auto_resets_subproperties() {
  let dom = dom::parse_html(
    r#"<div style="text-decoration-skip: none; text-decoration-skip: auto"><span>child</span></div>"#,
  )
  .unwrap();
  let stylesheet = parse_stylesheet("").unwrap();
  let styled = apply_styles_with_media(&dom, &stylesheet, &MediaContext::screen(800.0, 600.0));

  let parent = find_first(&styled, "div").expect("div");
  let child = find_first(parent, "span").expect("span");

  assert_eq!(parent.styles.text_decoration_skip_self, TextDecorationSkipSelf::Auto);
  assert_eq!(parent.styles.text_decoration_skip_box, TextDecorationSkipBox::None);
  assert_eq!(
    parent.styles.text_decoration_skip_spaces,
    TextDecorationSkipSpaces::StartEnd
  );
  assert_eq!(parent.styles.text_decoration_skip_ink, TextDecorationSkipInk::Auto);

  assert_eq!(child.styles.text_decoration_skip_self, TextDecorationSkipSelf::Auto);
  assert_eq!(child.styles.text_decoration_skip_box, TextDecorationSkipBox::None);
  assert_eq!(
    child.styles.text_decoration_skip_spaces,
    TextDecorationSkipSpaces::StartEnd
  );
  assert_eq!(child.styles.text_decoration_skip_ink, TextDecorationSkipInk::Auto);
}

#[test]
fn webkit_text_decoration_skip_objects_aliases_to_text_decoration_skip() {
  // Ensure `objects` is accepted and applied through the existing vendor-prefixed aliasing.
  // Start from a non-initial value so the `objects` keyword actually changes computed state.
  let dom = dom::parse_html(
    r#"<div style="text-decoration-skip: none; -webkit-text-decoration-skip: objects"></div>"#,
  )
  .unwrap();
  let stylesheet = parse_stylesheet("").unwrap();
  let styled = apply_styles_with_media(&dom, &stylesheet, &MediaContext::screen(800.0, 600.0));
  let node = find_first(&styled, "div").expect("div");

  assert_eq!(node.styles.text_decoration_skip_self, TextDecorationSkipSelf::Auto);
  assert_eq!(node.styles.text_decoration_skip_box, TextDecorationSkipBox::None);
  assert_eq!(
    node.styles.text_decoration_skip_spaces,
    TextDecorationSkipSpaces::StartEnd
  );
  assert_eq!(node.styles.text_decoration_skip_ink, TextDecorationSkipInk::Auto);
}

#[test]
fn webkit_text_decoration_skip_ink_aliases_to_text_decoration_skip() {
  // Web-compat: `-webkit-text-decoration-skip: ink` is common in CSS resets for legacy Safari.
  // Start from a non-initial value so the `ink` keyword actually changes computed state.
  let dom =
    dom::parse_html(r#"<div style="text-decoration-skip: none; -webkit-text-decoration-skip: ink"></div>"#)
      .unwrap();
  let stylesheet = parse_stylesheet("").unwrap();
  let styled = apply_styles_with_media(&dom, &stylesheet, &MediaContext::screen(800.0, 600.0));
  let node = find_first(&styled, "div").expect("div");

  assert_eq!(node.styles.text_decoration_skip_self, TextDecorationSkipSelf::Auto);
  assert_eq!(node.styles.text_decoration_skip_box, TextDecorationSkipBox::None);
  assert_eq!(
    node.styles.text_decoration_skip_spaces,
    TextDecorationSkipSpaces::StartEnd
  );
  assert_eq!(node.styles.text_decoration_skip_ink, TextDecorationSkipInk::Auto);
}

#[test]
fn text_decoration_skip_self_does_not_inherit() {
  let dom = dom::parse_html(
    r#"<div style="text-decoration-skip-self: no-skip"><span>child</span></div>"#,
  )
  .unwrap();
  let stylesheet = parse_stylesheet("").unwrap();
  let styled = apply_styles_with_media(&dom, &stylesheet, &MediaContext::screen(800.0, 600.0));

  let parent = find_first(&styled, "div").expect("div");
  let child = find_first(parent, "span").expect("span");

  assert_eq!(parent.styles.text_decoration_skip_self, TextDecorationSkipSelf::NoSkip);
  assert_eq!(child.styles.text_decoration_skip_self, TextDecorationSkipSelf::Auto);
}

#[test]
fn text_decoration_skip_spaces_inherits() {
  let dom = dom::parse_html(
    r#"<div style="text-decoration-skip-spaces: none"><span>child</span></div>"#,
  )
  .unwrap();
  let stylesheet = parse_stylesheet("").unwrap();
  let styled = apply_styles_with_media(&dom, &stylesheet, &MediaContext::screen(800.0, 600.0));

  let parent = find_first(&styled, "div").expect("div");
  let child = find_first(parent, "span").expect("span");

  assert_eq!(
    parent.styles.text_decoration_skip_spaces,
    TextDecorationSkipSpaces::None
  );
  assert_eq!(
    child.styles.text_decoration_skip_spaces,
    TextDecorationSkipSpaces::None
  );
}

#[test]
fn text_decoration_skip_spaces_parses_in_stylesheets() {
  let dom = dom::parse_html(r#"<div class="sample"><span>child</span></div>"#).unwrap();
  let stylesheet = parse_stylesheet(".sample { text-decoration-skip-spaces: none; }").unwrap();
  let styled = apply_styles_with_media(&dom, &stylesheet, &MediaContext::screen(800.0, 600.0));

  let parent = find_first(&styled, "div").expect("div");
  let child = find_first(parent, "span").expect("span");

  assert_eq!(
    parent.styles.text_decoration_skip_spaces,
    TextDecorationSkipSpaces::None
  );
  assert_eq!(
    child.styles.text_decoration_skip_spaces,
    TextDecorationSkipSpaces::None
  );
}
