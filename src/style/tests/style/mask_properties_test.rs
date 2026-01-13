use crate::css::parser::parse_stylesheet;
use crate::dom;
use crate::style::cascade::apply_styles_with_media;
use crate::style::cascade::StyledNode;
use crate::style::media::MediaContext;
use crate::style::types::{
  BackgroundImage, BackgroundPosition, BackgroundRepeatKeyword, BackgroundSize,
  BackgroundSizeComponent, BackgroundSizeKeyword, MaskClip, MaskComposite, MaskMode, MaskOrigin,
};
use crate::style::values::Length;

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
fn mask_longhands_ignore_extra_layer_values() {
  let dom = dom::parse_html(
    r#"<div
      style="mask-image: linear-gradient(black, black);
             mask-clip: padding-box, content-box;
             mask-origin: content-box;
             mask-composite: intersect;"
    ></div>"#,
  )
  .unwrap();
  let stylesheet = parse_stylesheet("").unwrap();
  let styled = apply_styles_with_media(&dom, &stylesheet, &MediaContext::screen(800.0, 600.0));

  let style = &find_first(&styled, "div").expect("div").styles;
  assert_eq!(
    style.mask_layers.len(),
    1,
    "layer count should follow mask-image"
  );

  assert_eq!(style.mask_layers[0].clip, MaskClip::PaddingBox);
  assert_eq!(style.mask_layers[0].origin, MaskOrigin::ContentBox);
  assert_eq!(style.mask_layers[0].composite, MaskComposite::Intersect);
  assert!(
    style.mask_layers.iter().all(|layer| layer.image.is_some()),
    "mask-image should be present for the single layer"
  );
}

#[test]
fn mask_none_ignores_extra_layer_values() {
  let dom =
    dom::parse_html(r#"<div style="mask-image: none; mask-clip: padding-box, border-box"></div>"#)
      .unwrap();
  let stylesheet = parse_stylesheet("").unwrap();
  let styled = apply_styles_with_media(&dom, &stylesheet, &MediaContext::screen(800.0, 600.0));

  let style = &find_first(&styled, "div").expect("div").styles;
  assert_eq!(style.mask_layers.len(), 1);
  assert!(style.mask_layers.iter().all(|layer| layer.image.is_none()));
  assert_eq!(style.mask_layers[0].clip, MaskClip::PaddingBox);
}

#[test]
fn mask_shorthand_parses_position_size_and_repeat() {
  let dom =
    dom::parse_html(r#"<div style="mask: url(a) center/contain no-repeat;"></div>"#).unwrap();
  let stylesheet = parse_stylesheet("").unwrap();
  let styled = apply_styles_with_media(&dom, &stylesheet, &MediaContext::screen(800.0, 600.0));

  let layer = &find_first(&styled, "div").expect("div").styles.mask_layers[0];
  assert!(matches!(
    layer.image,
    Some(BackgroundImage::Url(ref url)) if url.url == "a" && url.override_resolution.is_none()
  ));
  let BackgroundPosition::Position { x, y } = &layer.position;
  assert!((x.alignment - 0.5).abs() < 1e-6);
  assert_eq!(x.offset, Length::percent(0.0));
  assert!((y.alignment - 0.5).abs() < 1e-6);
  assert_eq!(y.offset, Length::percent(0.0));
  assert_eq!(
    layer.size,
    BackgroundSize::Keyword(BackgroundSizeKeyword::Contain)
  );
  assert_eq!(layer.repeat.x, BackgroundRepeatKeyword::NoRepeat);
  assert_eq!(layer.repeat.y, BackgroundRepeatKeyword::NoRepeat);
}

#[test]
fn mask_shorthand_sets_origin_and_clip() {
  let dom =
    dom::parse_html(r#"<div style="mask: url(a) content-box padding-box;"></div>"#).unwrap();
  let stylesheet = parse_stylesheet("").unwrap();
  let styled = apply_styles_with_media(&dom, &stylesheet, &MediaContext::screen(800.0, 600.0));

  let layer = &find_first(&styled, "div").expect("div").styles.mask_layers[0];
  assert_eq!(layer.origin, MaskOrigin::ContentBox);
  assert_eq!(layer.clip, MaskClip::PaddingBox);
}

#[test]
fn mask_layers_repeat_longhands_to_match_images() {
  let dom =
    dom::parse_html(r#"<div style="mask: url(a), url(b); mask-repeat: repeat-x;"></div>"#).unwrap();
  let stylesheet = parse_stylesheet("").unwrap();
  let styled = apply_styles_with_media(&dom, &stylesheet, &MediaContext::screen(800.0, 600.0));

  let style = &find_first(&styled, "div").expect("div").styles;
  assert_eq!(style.mask_layers.len(), 2);
  assert!(matches!(
    style.mask_layers[0].image,
    Some(BackgroundImage::Url(ref url)) if url.url == "a" && url.override_resolution.is_none()
  ));
  assert!(matches!(
    style.mask_layers[1].image,
    Some(BackgroundImage::Url(ref url)) if url.url == "b" && url.override_resolution.is_none()
  ));
  for layer in &style.mask_layers {
    assert_eq!(layer.repeat.x, BackgroundRepeatKeyword::Repeat);
    assert_eq!(layer.repeat.y, BackgroundRepeatKeyword::NoRepeat);
  }
}

#[test]
fn mask_shorthand_resets_mask_size_when_omitted() {
  let dom = dom::parse_html(r#"<div class="icon icon--heart"></div>"#).unwrap();
  let stylesheet = parse_stylesheet(
    r#"
      .icon { mask-size: cover; }
      .icon--heart { mask: url(a) no-repeat center; }
    "#,
  )
  .unwrap();
  let styled = apply_styles_with_media(&dom, &stylesheet, &MediaContext::screen(800.0, 600.0));

  let layer = &find_first(&styled, "div").expect("div").styles.mask_layers[0];
  assert_eq!(
    layer.size,
    BackgroundSize::Explicit(BackgroundSizeComponent::Auto, BackgroundSizeComponent::Auto),
    "mask shorthand without an explicit size should reset mask-size to its initial value"
  );
}

#[test]
fn webkit_mask_longhands_alias_to_unprefixed() {
  // Real-world sites frequently ship `-webkit-mask-*` fallbacks (sometimes without the unprefixed
  // spelling). Ensure the parser canonicalizes these to the standardized `mask-*` properties so
  // they apply identically.
  let dom = dom::parse_html(r#"<div class="target"></div>"#).unwrap();
  let stylesheet = parse_stylesheet(
    r#"
      .target {
        -webkit-mask-image: url(a);
        -webkit-mask-size: 10px 20px;
        -webkit-mask-repeat: no-repeat;
        -webkit-mask-position: 10px 20px;
        -webkit-mask-mode: luminance;
      }
    "#,
  )
  .unwrap();
  let styled = apply_styles_with_media(&dom, &stylesheet, &MediaContext::screen(800.0, 600.0));

  let style = &find_first(&styled, "div").expect("div").styles;
  assert_eq!(style.mask_images.len(), 1);
  let image = style.mask_images[0].clone();
  assert!(matches!(
    image,
    Some(BackgroundImage::Url(ref url)) if url.url == "a" && url.override_resolution.is_none()
  ));

  assert_eq!(
    style.mask_sizes[0],
    BackgroundSize::Explicit(
      BackgroundSizeComponent::Length(Length::px(10.0)),
      BackgroundSizeComponent::Length(Length::px(20.0)),
    )
  );
  assert_eq!(style.mask_repeats[0].x, BackgroundRepeatKeyword::NoRepeat);
  assert_eq!(style.mask_repeats[0].y, BackgroundRepeatKeyword::NoRepeat);

  let BackgroundPosition::Position { x, y } = style.mask_positions[0];
  assert_eq!(x.alignment, 0.0);
  assert_eq!(x.offset, Length::px(10.0));
  assert_eq!(y.alignment, 0.0);
  assert_eq!(y.offset, Length::px(20.0));

  assert_eq!(style.mask_modes[0], MaskMode::Luminance);
}
