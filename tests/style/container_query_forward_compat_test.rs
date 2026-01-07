use fastrender::css::parser::parse_stylesheet;
use fastrender::css::types::{ContainerQuery, CssRule};
use fastrender::style::media::MediaContext;

#[test]
fn unknown_container_queries_do_not_drop_nested_keyframes() {
  let css = r#"
    @container scroll-state(stuck: top) {
      @keyframes spin {
        from { transform: rotate(0deg); }
        to { transform: rotate(360deg); }
      }
    }
  "#;

  let stylesheet = parse_stylesheet(css).expect("parsed stylesheet");
  let container_rule = stylesheet
    .rules
    .iter()
    .find_map(|rule| match rule {
      CssRule::Container(container) => Some(container),
      _ => None,
    })
    .expect("@container rule retained");

  assert_eq!(container_rule.conditions.len(), 1);
  assert!(matches!(
    container_rule.conditions.first().and_then(|condition| condition.query.as_ref()),
    Some(ContainerQuery::Unknown(raw)) if raw.starts_with("scroll-state(")
  ));

  let keyframes = stylesheet.collect_keyframes(&MediaContext::screen(800.0, 600.0));
  assert!(keyframes.iter().any(|kf| kf.name == "spin"));
}

#[test]
fn grouped_container_queries_parse() {
  let css = r#"
    @container ((min-width: 200px) and (max-width: 400px)) {
      .target { display: inline; }
    }
  "#;

  let stylesheet = parse_stylesheet(css).expect("parsed stylesheet");
  let container_rule = stylesheet
    .rules
    .iter()
    .find_map(|rule| match rule {
      CssRule::Container(container) => Some(container),
      _ => None,
    })
    .expect("@container rule");

  assert_eq!(container_rule.conditions.len(), 1);
  match container_rule.conditions[0]
    .query
    .as_ref()
    .expect("container query") {
    ContainerQuery::And(list) => {
      assert_eq!(list.len(), 2);
      assert!(matches!(list[0], ContainerQuery::Size(_)));
      assert!(matches!(list[1], ContainerQuery::Size(_)));
    }
    other => panic!("expected grouped query to parse as And, got {other:?}"),
  }
}
