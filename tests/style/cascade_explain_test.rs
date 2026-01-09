use fastrender::css::parser::parse_stylesheet;
use fastrender::dom;
use fastrender::style::cascade::{apply_styles_with_media, explain_property_for_node, StyledNode};
use fastrender::style::color::Rgba;
use fastrender::style::media::MediaContext;

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

#[test]
fn explain_property_matches_cascade_winner_and_ordering() {
  let dom = dom::parse_html(r#"<div id="target" class="item"></div>"#).expect("parse html");
  let css = r#"
    @layer a, b;

    @layer a {
      #target { color: rgb(1, 2, 3); }
      .item { color: rgb(10, 11, 12) !important; }
    }

    @layer b {
      .item { color: rgb(4, 5, 6); }
      #target { color: rgb(7, 8, 9) !important; }
    }
  "#;
  let sheet = parse_stylesheet(css).expect("parse stylesheet");
  let media = MediaContext::screen(800.0, 600.0);

  let styled = apply_styles_with_media(&dom, &sheet, &media);
  let target = find_by_id(&styled, "target").expect("target node");
  assert_eq!(target.styles.color, Rgba::rgb(10, 11, 12));

  let explain =
    explain_property_for_node(&dom, &sheet, &media, None, target.node_id, "color").expect("explain");
  assert_eq!(explain.winner, Some(3));
  assert_eq!(explain.candidates_in_cascade_order.len(), 4);

  let selectors: Vec<&str> = explain
    .candidates_in_cascade_order
    .iter()
    .map(|c| c.selector.as_str())
    .collect();
  assert_eq!(selectors, vec!["#target", ".item", "#target", ".item"]);

  let values: Vec<&str> = explain
    .candidates_in_cascade_order
    .iter()
    .map(|c| c.value.as_str())
    .collect();
  assert_eq!(
    values,
    vec![
      "rgb(1, 2, 3)",
      "rgb(4, 5, 6)",
      "rgb(7, 8, 9)",
      "rgb(10, 11, 12)",
    ]
  );

  let winner = explain.winner.expect("winner index");
  assert_eq!(
    explain.candidates_in_cascade_order[winner].value,
    target.styles.color.to_string()
  );
}

