use crate::css::parser::parse_stylesheet;
use crate::dom;
use crate::style::cascade::apply_styles_with_media;
use crate::style::cascade::StyledNode;
use crate::style::media::MediaContext;
use crate::style::types::{
  BackgroundImage, BackgroundRepeatKeyword, BackgroundSize, BackgroundSizeKeyword,
};

fn find_by_id<'a>(node: &'a StyledNode, id: &str) -> Option<&'a StyledNode> {
  if node
    .node
    .get_attribute_ref("id")
    .is_some_and(|value| value == id)
  {
    return Some(node);
  }
  for child in node.children.iter() {
    if let Some(found) = find_by_id(child, id) {
      return Some(found);
    }
  }
  None
}

#[test]
fn webkit_mask_longhands_alias_to_unprefixed_properties() {
  let dom = dom::parse_html(
    r#"<div id=t style="-webkit-mask-image: url(a); -webkit-mask-size: cover; -webkit-mask-repeat: no-repeat"></div>"#,
  )
  .unwrap();
  let stylesheet = parse_stylesheet("").unwrap();
  let styled = apply_styles_with_media(&dom, &stylesheet, &MediaContext::screen(800.0, 600.0));

  let styles = &find_by_id(&styled, "t").expect("#t").styles;
  assert_eq!(styles.mask_layers.len(), 1);

  assert!(matches!(
    styles.mask_layers[0].image,
    Some(BackgroundImage::Url(ref url)) if url.url == "a" && url.override_resolution.is_none()
  ));
  assert_eq!(
    styles.mask_layers[0].size,
    BackgroundSize::Keyword(BackgroundSizeKeyword::Cover)
  );
  assert_eq!(styles.mask_layers[0].repeat.x, BackgroundRepeatKeyword::NoRepeat);
  assert_eq!(styles.mask_layers[0].repeat.y, BackgroundRepeatKeyword::NoRepeat);
}

#[test]
fn supports_query_with_webkit_mask_image_matches_and_applies_rule() {
  let dom = dom::parse_html(r#"<div id=t></div>"#).unwrap();
  let stylesheet = parse_stylesheet(
    r#"
      #t { mask-image: none; }
      @supports (-webkit-mask-image: none) {
        #t { -webkit-mask-image: url(a); }
      }
    "#,
  )
  .unwrap();
  let styled = apply_styles_with_media(&dom, &stylesheet, &MediaContext::screen(800.0, 600.0));

  let styles = &find_by_id(&styled, "t").expect("#t").styles;
  assert_eq!(styles.mask_layers.len(), 1);
  assert!(matches!(
    styles.mask_layers[0].image,
    Some(BackgroundImage::Url(ref url)) if url.url == "a" && url.override_resolution.is_none()
  ));
}

