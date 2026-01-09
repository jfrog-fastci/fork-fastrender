use fastrender::css::parser::parse_stylesheet;
use fastrender::css::types::PropertyValue;
use fastrender::dom;
use fastrender::style::cascade::{apply_styles_with_media, StyledNode};
use fastrender::style::media::MediaContext;
use fastrender::style::types::PositionTryOrder;
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
fn supports_position_try_fallbacks_builtin_flip_block() {
  let target = styled_target(
    r#"
      @supports (position-try-fallbacks: flip-block) {
        #t { color: rgb(11, 12, 13); }
      }
      @supports not (position-try-fallbacks: flip-block) {
        #t { color: rgb(1, 1, 1); }
      }
    "#,
  );

  assert_eq!(target.styles.color, Rgba::rgb(11, 12, 13));
}

#[test]
fn supports_position_try_fallbacks_multiple_tactics() {
  let target = styled_target(
    r#"
      @supports (position-try-fallbacks: flip-inline flip-block) {
        #t { color: rgb(21, 22, 23); }
      }
      @supports not (position-try-fallbacks: flip-inline flip-block) {
        #t { color: rgb(1, 1, 1); }
      }
    "#,
  );

  assert_eq!(target.styles.color, Rgba::rgb(21, 22, 23));
}

#[test]
fn supports_position_try_fallbacks_flip_x_and_flip_y() {
  let target = styled_target(
    r#"
      @supports (position-try-fallbacks: flip-x, flip-y) {
        #t { color: rgb(31, 32, 33); }
      }
      @supports not (position-try-fallbacks: flip-x, flip-y) {
        #t { color: rgb(1, 1, 1); }
      }
    "#,
  );

  assert_eq!(target.styles.color, Rgba::rgb(31, 32, 33));
}

#[test]
fn supports_position_try_fallbacks_dashed_ident_and_tactic() {
  let target = styled_target(
    r#"
      @supports (position-try-fallbacks: flip-block --foo) {
        #t { color: rgb(41, 42, 43); }
      }
      @supports not (position-try-fallbacks: flip-block --foo) {
        #t { color: rgb(1, 1, 1); }
      }
    "#,
  );

  assert_eq!(target.styles.color, Rgba::rgb(41, 42, 43));
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
fn position_try_fallbacks_parses_multiple_tactics_per_fallback() {
  let target = styled_target(
    r#"
      #t { position-try-fallbacks: flip-inline/*comment*/flip-block; }
    "#,
  );

  assert_eq!(
    target.styles.position_try_fallbacks,
    vec!["flip-inline flip-block".to_string()]
  );
}

#[test]
fn position_try_fallbacks_parses_dashed_ident_with_try_tactic() {
  let target = styled_target(
    r#"
       #t { position-try-fallbacks: flip-block --foo; }
    "#,
  );

  assert_eq!(
    target.styles.position_try_fallbacks,
    vec!["--foo flip-block".to_string()]
  );
}

#[test]
fn supports_position_try_order_property() {
  let target = styled_target(
    r#"
      @supports (position-try-order: most-width) {
        #t { color: rgb(51, 52, 53); }
      }
      @supports not (position-try-order: most-width) {
        #t { color: rgb(1, 1, 1); }
      }
    "#,
  );

  assert_eq!(target.styles.color, Rgba::rgb(51, 52, 53));
}

#[test]
fn position_try_order_parses_most_height_keyword() {
  let target = styled_target(
    r#"
      #t { position-try-order: most-height; }
    "#,
  );

  assert_eq!(target.styles.position_try_order, PositionTryOrder::MostHeight);
}

#[test]
fn supports_position_try_shorthand() {
  let target = styled_target(
    r#"
      @supports (position-try: most-inline-size flip-inline) {
        #t { color: rgb(61, 62, 63); }
      }
      @supports not (position-try: most-inline-size flip-inline) {
        #t { color: rgb(1, 1, 1); }
      }
    "#,
  );

  assert_eq!(target.styles.color, Rgba::rgb(61, 62, 63));
}

#[test]
fn position_try_shorthand_does_not_support_order_last() {
  let target = styled_target(
    r#"
      @supports (position-try: flip-inline most-inline-size) {
        #t { color: rgb(1, 1, 1); }
      }
      @supports not (position-try: flip-inline most-inline-size) {
        #t { color: rgb(71, 72, 73); }
      }
    "#,
  );

  assert_eq!(target.styles.color, Rgba::rgb(71, 72, 73));
}

#[test]
fn position_try_shorthand_rejects_order_last_with_comment_separator() {
  let target = styled_target(
    r#"
      #t { position-try: flip-inline/*comment*/most-width; }
    "#,
  );

  assert_eq!(target.styles.position_try_order, PositionTryOrder::Normal);
  assert!(target.styles.position_try_fallbacks.is_empty());
}

#[test]
fn position_try_shorthand_does_not_support_order_only() {
  let target = styled_target(
    r#"
      @supports (position-try: most-width) {
        #t { color: rgb(1, 1, 1); }
      }
      @supports not (position-try: most-width) {
        #t { color: rgb(81, 82, 83); }
      }
    "#,
  );

  assert_eq!(target.styles.color, Rgba::rgb(81, 82, 83));
}

#[test]
fn position_try_shorthand_rejects_order_only_in_declaration() {
  let target = styled_target(
    r#"
      #t { position-try: most-width; }
    "#,
  );

  assert_eq!(target.styles.position_try_order, PositionTryOrder::Normal);
  assert!(target.styles.position_try_fallbacks.is_empty());
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
        /* Disallowed properties and !important declarations should be ignored. */
        @position-try --flip { left: 2px; color: rgb(9, 9, 9); margin-left: 5px !important; }
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

#[test]
fn position_try_rules_keep_only_positioning_related_properties() {
  let target = styled_target(
    r#"
      @position-try --ok {
        /* accepted */
        top: 1px;
        position-anchor: --a;
        margin-left: 2px;
        width: 3px;
        /* rejected */
        color: rgb(1, 2, 3);
        background: red;
      }
    "#,
  );

  let decls = target
    .styles
    .position_try_registry
    .get("--ok")
    .expect("expected @position-try rule to be collected");
  assert!(
    decls.iter().all(|decl| decl.property.as_ref() != "color" && decl.property.as_ref() != "background"),
    "unexpected non-positioning properties in @position-try registry: {decls:?}"
  );
  assert!(decls.iter().any(|decl| decl.property.as_ref() == "top"));
  assert!(decls.iter().any(|decl| decl.property.as_ref() == "position-anchor"));
  assert!(decls.iter().any(|decl| decl.property.as_ref() == "margin-left"));
  assert!(decls.iter().any(|decl| decl.property.as_ref() == "width"));
}
