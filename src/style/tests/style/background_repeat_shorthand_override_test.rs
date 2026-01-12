use crate::css::parser::parse_stylesheet;
use crate::dom;
use crate::style::cascade::apply_styles_with_media;
use crate::style::cascade::StyledNode;
use crate::style::media::MediaContext;
use crate::style::types::BackgroundRepeatKeyword;
use crate::Size;

fn find_first_div<'a>(node: &'a StyledNode) -> Option<&'a StyledNode> {
  if node
    .node
    .tag_name()
    .is_some_and(|tag| tag.eq_ignore_ascii_case("div"))
  {
    return Some(node);
  }
  for child in node.children.iter() {
    if let Some(found) = find_first_div(child) {
      return Some(found);
    }
  }
  None
}

#[test]
fn background_repeat_longhand_overrides_background_shorthand() {
  let dom = dom::parse_html(
    r#"<div style="background: linear-gradient(red, red), linear-gradient(green, green); background-repeat: repeat-y;"></div>"#,
  )
  .unwrap();
  let stylesheet = parse_stylesheet("").unwrap();
  let styled = apply_styles_with_media(&dom, &stylesheet, &MediaContext::screen(1000.0, 600.0));
  let div = find_first_div(&styled).expect("expected div");

  // `background-repeat: repeat-y` is equivalent to `no-repeat repeat` and should override the
  // implicit `repeat` values from the `background` shorthand.
  for layer in div.styles.background_layers.iter() {
    assert_eq!(
      layer.repeat.x,
      BackgroundRepeatKeyword::NoRepeat,
      "repeat-y should disable horizontal repetition"
    );
    assert_eq!(
      layer.repeat.y,
      BackgroundRepeatKeyword::Repeat,
      "repeat-y should repeat vertically"
    );
  }
}

#[test]
fn background_repeat_applies_to_pseudo_element_inside_media_query() {
  let dom = dom::parse_html(r#"<div class="Gradient"></div>"#).unwrap();
  let css = r#"
    .Gradient:after { content: ""; }
    @media (min-width: 900px) {
      .Gradient:after {
        background: radial-gradient(red 10%, transparent 50% 100%) 520px -250px, blue;
        background-repeat: repeat-y
      }
    }
  "#;
  let stylesheet = parse_stylesheet(css).unwrap();
  let styled = apply_styles_with_media(&dom, &stylesheet, &MediaContext::screen(1000.0, 600.0));
  let div = find_first_div(&styled).expect("expected div");
  let after = div.after_styles.as_ref().expect("expected ::after styles");

  for layer in after.background_layers.iter() {
    assert_eq!(layer.repeat.x, BackgroundRepeatKeyword::NoRepeat);
    assert_eq!(layer.repeat.y, BackgroundRepeatKeyword::Repeat);
  }
}

#[test]
fn background_repeat_survives_minified_media_block_followed_by_selector() {
  // Stripe.com uses a fully minified `<style>` block where the `@media` rule is immediately
  // followed by another selector with no whitespace: `...background-repeat:repeat-y}}.Next{...}`.
  // Ensure we correctly terminate the declaration at the end of the rule block.
  let dom = dom::parse_html(r#"<div class="Gradient isLoaded"></div>"#).unwrap();
  let css = r#"
    .Gradient:after{content:""}
    @media (min-width:900px){.Gradient:after{background:radial-gradient(red 23%,transparent 67% 100%) 385px -24px,radial-gradient(blue 0,transparent 60% 100%) -940px 290px,black;background-repeat:repeat-y}}.Gradient.isLoaded:after{transform:translateX(-50%) scaleY(.995)}
  "#;
  let stylesheet = parse_stylesheet(css).unwrap();
  let styled = apply_styles_with_media(&dom, &stylesheet, &MediaContext::screen(1000.0, 600.0));
  let div = find_first_div(&styled).expect("expected div");
  let after = div.after_styles.as_ref().expect("expected ::after styles");

  assert!(
    after.background_layers.len() >= 2,
    "expected multiple background layers from the shorthand"
  );
  for layer in after.background_layers.iter() {
    assert_eq!(layer.repeat.x, BackgroundRepeatKeyword::NoRepeat);
    assert_eq!(layer.repeat.y, BackgroundRepeatKeyword::Repeat);
  }
}

#[test]
fn background_repeat_survives_var_dependent_recompute() {
  // When a `background` shorthand contains `var()`, we recompute it at paint time after custom
  // property inheritance/animation. That recomputation must not clobber later background-* longhands
  // such as `background-repeat`.
  let dom = dom::parse_html(r#"<div class="Gradient" style="--c0: red;"></div>"#).unwrap();
  let css = r#"
    .Gradient:after {
      content: "";
      background: radial-gradient(var(--c0) 0%, transparent 100%) 0 0;
      background-repeat: repeat-y;
    }
  "#;
  let stylesheet = parse_stylesheet(css).unwrap();
  let viewport = Size::new(1000.0, 600.0);
  let styled = apply_styles_with_media(
    &dom,
    &stylesheet,
    &MediaContext::screen(viewport.width, viewport.height),
  );
  let div = find_first_div(&styled).expect("expected div");
  let after = div.after_styles.as_ref().expect("expected ::after styles");

  for layer in after.background_layers.iter() {
    assert_eq!(layer.repeat.x, BackgroundRepeatKeyword::NoRepeat);
    assert_eq!(layer.repeat.y, BackgroundRepeatKeyword::Repeat);
  }

  let mut recomputed = after.as_ref().clone();
  recomputed.recompute_var_dependent_properties(div.styles.as_ref(), viewport);

  for layer in recomputed.background_layers.iter() {
    assert_eq!(
      layer.repeat.x,
      BackgroundRepeatKeyword::NoRepeat,
      "background-repeat should keep horizontal repetition disabled after recompute"
    );
    assert_eq!(
      layer.repeat.y,
      BackgroundRepeatKeyword::Repeat,
      "background-repeat should keep vertical repetition enabled after recompute"
    );
  }
}
