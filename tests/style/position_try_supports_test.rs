use fastrender::css::parser::parse_stylesheet;
use fastrender::css::types::PropertyValue;
use fastrender::dom;
use fastrender::style::cascade::{apply_styles_with_media, StyledNode};
use fastrender::style::media::MediaContext;
use fastrender::style::values::Length;
use fastrender::Rgba;

fn find_by_id<'a>(node: &'a StyledNode, id: &str) -> Option<&'a StyledNode> {
  if node
    .node
    .get_attribute_ref("id")
    .map(|value| value == id)
    .unwrap_or(false)
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

fn styled_target(css: &str) -> StyledNode {
  let media = MediaContext::screen(800.0, 600.0);
  let stylesheet = parse_stylesheet(css).unwrap();
  let dom = dom::parse_html(r#"<div id="t"></div>"#).unwrap();
  let styled = apply_styles_with_media(&dom, &stylesheet, &media);
  find_by_id(&styled, "t")
    .expect("target element should be styled")
    .clone()
}

#[test]
fn supports_position_try_fallbacks_property() {
  let target = styled_target(
    r#"
      @supports (position-try-fallbacks: --flip) {
        #t { color: rgb(1, 2, 3); }
      }
      @supports not (position-try-fallbacks: --flip) {
        #t { color: rgb(9, 9, 9); }
      }
    "#,
  );

  assert_eq!(target.styles.color, Rgba::rgb(1, 2, 3));
}

#[test]
fn supports_position_try_fallbacks_builtin_keywords() {
  let target = styled_target(
    r#"
      @supports (position-try-fallbacks: flip-inline) {
        #t { color: rgb(7, 8, 9); }
      }
      @supports not (position-try-fallbacks: flip-inline) {
        #t { color: rgb(1, 1, 1); }
      }
    "#,
  );

  assert_eq!(target.styles.color, Rgba::rgb(7, 8, 9));
}

#[test]
fn position_try_fallbacks_parses_and_skips_comments() {
  let target = styled_target(
    r#"
      #t { position-try-fallbacks: flip-inline/*comment*/, --foo; }
    "#,
  );

  assert_eq!(
    target.styles.position_try_fallbacks,
    vec!["flip-inline".to_string(), "--foo".to_string()]
  );
}

#[test]
fn supports_position_try_at_rule() {
  let target = styled_target(
    r#"
      @supports at-rule(@position-try) {
        #t { color: rgb(4, 5, 6); }
      }
      @supports not at-rule(@position-try) {
        #t { color: rgb(9, 9, 9); }
      }
    "#,
  );

  assert_eq!(target.styles.color, Rgba::rgb(4, 5, 6));
}

#[test]
fn position_try_rules_follow_layer_ordering() {
  let target = styled_target(
    r#"
      @layer a {
        @position-try --flip { left: 1px; }
      }
      @layer b {
        @position-try --flip { left: 2px; }
      }
    "#,
  );

  let decls = target
    .styles
    .position_try_registry
    .get("--flip")
    .expect("expected @position-try rule to be collected");
  assert_eq!(decls.len(), 1);
  assert_eq!(decls[0].property.as_ref(), "left");
  match &decls[0].value {
    PropertyValue::Length(len) => assert_eq!(*len, Length::px(2.0)),
    other => panic!("expected left: <length>, got {other:?}"),
  }
}
