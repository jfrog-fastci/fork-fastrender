use fastrender::css::parser::parse_stylesheet;
use fastrender::dom;
use fastrender::style::cascade::apply_styles_with_media_target_and_imports;
use fastrender::style::cascade::StyledNode;
use fastrender::style::color::Rgba;
use fastrender::style::media::MediaContext;
use fastrender::style::types::{BackgroundImage, BackgroundImageUrl, CalcSizeBasis, IntrinsicSizeKeyword};
use fastrender::style::values::Length;

fn find_by_id<'a>(node: &'a StyledNode, id: &str) -> Option<&'a StyledNode> {
  if node
    .node
    .get_attribute_ref("id")
    .is_some_and(|value| value.eq_ignore_ascii_case(id))
  {
    return Some(node);
  }
  node.children.iter().find_map(|child| find_by_id(child, id))
}

fn styled_for(css: &str, html: &str, media: &MediaContext) -> StyledNode {
  let dom = dom::parse_html(html).unwrap();
  let stylesheet = parse_stylesheet(css).unwrap();
  apply_styles_with_media_target_and_imports(
    &dom,
    &stylesheet,
    media,
    None,
    None,
    None,
    None,
    None,
    None,
  )
}

#[test]
fn if_media_selects_true_branch() {
  let css = r#"
    #t1 { color: if(media((min-width: 1px)): rgb(1, 2, 3); rgb(4, 5, 6)); }
  "#;
  let html = r#"<div id="t1"></div>"#;
  let media = MediaContext::screen(800.0, 600.0);
  let styled = styled_for(css, html, &media);
  let t1 = find_by_id(&styled, "t1").expect("t1");
  assert_eq!(t1.styles.color, Rgba::new(1, 2, 3, 1.0));
}

#[test]
fn if_media_selects_else_branch_and_is_lazy() {
  // The unselected branch contains an unresolved var(). Ensure it does not invalidate the chosen
  // branch.
  let css = r#"
    #t1 { color: if(media((min-width: 1000px)): var(--missing); rgb(1, 2, 3)); }
  "#;
  let html = r#"<div id="t1"></div>"#;
  let media = MediaContext::screen(800.0, 600.0);
  let styled = styled_for(css, html, &media);
  let t1 = find_by_id(&styled, "t1").expect("t1");
  assert_eq!(t1.styles.color, Rgba::new(1, 2, 3, 1.0));
}

#[test]
fn typed_attr_length_unit_and_fallback() {
  let css = r#"
    #t1 { width: attr(data-w px, 10px); }
    #t2 { width: attr(data-w px, 10px); }
  "#;
  let html = r#"
    <div id="t1" data-w="42"></div>
    <div id="t2"></div>
  "#;
  let media = MediaContext::screen(800.0, 600.0);
  let styled = styled_for(css, html, &media);
  let t1 = find_by_id(&styled, "t1").expect("t1");
  let t2 = find_by_id(&styled, "t2").expect("t2");
  assert_eq!(t1.styles.width, Some(Length::px(42.0)));
  assert_eq!(t2.styles.width, Some(Length::px(10.0)));
}

#[test]
fn typed_attr_integer_and_color() {
  let css = r#"
    #t1 {
      z-index: attr(data-z integer, 5);
      color: attr(data-c color, rgb(0, 0, 0));
    }
  "#;
  let html = "<div id=\"t1\" data-z=\"10\" data-c=\"#010203\"></div>";
  let media = MediaContext::screen(800.0, 600.0);
  let styled = styled_for(css, html, &media);
  let t1 = find_by_id(&styled, "t1").expect("t1");
  assert_eq!(t1.styles.z_index, Some(10));
  assert_eq!(t1.styles.color, Rgba::new(1, 2, 3, 1.0));
}

#[test]
fn typed_attr_url() {
  let css = r#"
    #t1 { background-image: attr(data-img url, none); }
    #t2 { background-image: attr(data-img url, none); }
  "#;
  let html = r#"
    <div id="t1" data-img="data:text/plain,foo'bar&quot;baz\qux"></div>
    <div id="t2"></div>
  "#;
  let media = MediaContext::screen(800.0, 600.0);
  let styled = styled_for(css, html, &media);
  let t1 = find_by_id(&styled, "t1").expect("t1");
  let t2 = find_by_id(&styled, "t2").expect("t2");

  assert_eq!(
    t1.styles.background_images.as_ref(),
    &[Some(BackgroundImage::Url(BackgroundImageUrl::new(
      "data:text/plain,foo'bar\"baz\\qux".to_string()
    )))],
  );
  assert_eq!(t2.styles.background_images.as_ref(), &[None]);
}

#[test]
fn typed_attr_url_uses_fallback_url_for_missing_or_empty_value() {
  let css = r#"
    #t1 { background-image: attr(data-bg url, url(fallback.png)); }
    #t2 { background-image: attr(data-bg url, url(fallback.png)); }
    #t3 { background-image: attr(data-bg url, url(fallback.png)); }
  "#;
  let html = r#"
    <div id="t1" data-bg="foo.png"></div>
    <div id="t2"></div>
    <div id="t3" data-bg=""></div>
  "#;
  let media = MediaContext::screen(800.0, 600.0);
  let styled = styled_for(css, html, &media);
  let t1 = find_by_id(&styled, "t1").expect("t1");
  let t2 = find_by_id(&styled, "t2").expect("t2");
  let t3 = find_by_id(&styled, "t3").expect("t3");

  assert_eq!(
    t1.styles.background_images.as_ref(),
    &[Some(BackgroundImage::Url(BackgroundImageUrl::new(
      "foo.png".to_string()
    )))],
  );
  assert_eq!(
    t2.styles.background_images.as_ref(),
    &[Some(BackgroundImage::Url(BackgroundImageUrl::new(
      "fallback.png".to_string()
    )))],
  );
  assert_eq!(
    t3.styles.background_images.as_ref(),
    &[Some(BackgroundImage::Url(BackgroundImageUrl::new(
      "fallback.png".to_string()
    )))],
  );
}

#[test]
fn calc_size_parses_as_intrinsic_size_keyword() {
  let dom = dom::parse_html(r#"<div id="t1"></div>"#).unwrap();
  let css = r#"#t1 { width: calc-size(auto, size - 10px); }"#;
  let stylesheet = parse_stylesheet(css).unwrap();
  let media = MediaContext::screen(800.0, 600.0);
  let styled = apply_styles_with_media_target_and_imports(
    &dom,
    &stylesheet,
    &media,
    None,
    None,
    None,
    None,
    None,
    None,
  );
  let t1 = find_by_id(&styled, "t1").expect("t1");
  match t1.styles.width_keyword {
    Some(IntrinsicSizeKeyword::CalcSize(calc)) => {
      assert!(matches!(calc.basis, CalcSizeBasis::Auto));
    }
    other => panic!("expected calc-size width keyword, got {other:?}"),
  }
}
