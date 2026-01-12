use crate::css::parser::parse_stylesheet;
use crate::dom;
use crate::style::cascade::apply_styles_with_media;
use crate::style::cascade::StyledNode;
use crate::style::media::MediaContext;
use crate::style::types::BackgroundRepeatKeyword;
use crate::{
  css::parser::parse_declarations, style::properties::apply_declaration,
  style::properties::DEFAULT_VIEWPORT, style::values::CustomPropertyValue, style::ComputedStyle,
};

fn find_first_div<'a>(node: &'a StyledNode) -> Option<&'a StyledNode> {
  if let Some(tag) = node.node.tag_name() {
    if tag.eq_ignore_ascii_case("div") {
      return Some(node);
    }
  }
  for child in node.children.iter() {
    if let Some(found) = find_first_div(child) {
      return Some(found);
    }
  }
  None
}

#[test]
fn background_repeat_repeat_y_overrides_background_shorthand_in_pseudo_element() {
  let dom = dom::parse_html(r#"<div class="Gradient"></div>"#).unwrap();
  let stylesheet = parse_stylesheet(
    r#"@media (min-width:900px){.Gradient:after{content:"";background:radial-gradient(red 40%,blue 60%) 385px -24px,green;background-repeat:repeat-y}}.Gradient.isLoaded:after{transform:none}"#,
  )
  .unwrap();
  let styled = apply_styles_with_media(&dom, &stylesheet, &MediaContext::screen(1200.0, 800.0));
  let div = find_first_div(&styled).expect("div");
  let after = div.after_styles.as_ref().expect("after styles");
  let rep = after
    .background_repeats
    .first()
    .copied()
    .expect("background repeat");
  assert_eq!(rep.x, BackgroundRepeatKeyword::NoRepeat);
  assert_eq!(rep.y, BackgroundRepeatKeyword::Repeat);
}

#[test]
fn background_shorthand_resets_background_repeat_when_applied_after_longhand() {
  let dom = dom::parse_html(r#"<div class="Gradient"></div>"#).unwrap();
  let stylesheet = parse_stylesheet(
    r#".Gradient:after{content:"";background-repeat:repeat-y;background:radial-gradient(red 40%,blue 60%) 385px -24px,green}"#,
  )
  .unwrap();
  let styled = apply_styles_with_media(&dom, &stylesheet, &MediaContext::screen(1200.0, 800.0));
  let div = find_first_div(&styled).expect("div");
  let after = div.after_styles.as_ref().expect("after styles");
  let rep = after
    .background_repeats
    .first()
    .copied()
    .expect("background repeat");
  assert_eq!(rep.x, BackgroundRepeatKeyword::Repeat);
  assert_eq!(rep.y, BackgroundRepeatKeyword::Repeat);
}

#[test]
fn recompute_var_dependent_background_does_not_clobber_background_repeat_longhand() {
  let mut style = ComputedStyle::default();
  let parent = ComputedStyle::default();
  let decls = parse_declarations("--bg: red; background: var(--bg); background-repeat: repeat-y;");
  for decl in decls.iter() {
    apply_declaration(&mut style, decl, &parent, 16.0, 16.0);
  }

  let rep = style
    .background_repeats
    .first()
    .copied()
    .expect("background repeat");
  assert_eq!(rep.x, BackgroundRepeatKeyword::NoRepeat);
  assert_eq!(rep.y, BackgroundRepeatKeyword::Repeat);

  // Mutate the custom property to force var-dependent recomputation.
  style
    .custom_properties
    .insert("--bg".into(), CustomPropertyValue::new("blue", None));
  style.recompute_var_dependent_properties(&parent, DEFAULT_VIEWPORT);

  let rep = style
    .background_repeats
    .first()
    .copied()
    .expect("background repeat");
  assert_eq!(rep.x, BackgroundRepeatKeyword::NoRepeat);
  assert_eq!(rep.y, BackgroundRepeatKeyword::Repeat);
}
