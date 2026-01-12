use fastrender::css::parser::parse_stylesheet;
use fastrender::dom;
use fastrender::style::cascade::{apply_styles_with_media, StyledNode};
use fastrender::style::media::MediaContext;
use fastrender::style::types::{GridAutoFlow, GridTrack};
use fastrender::style::values::Length;

fn find_by_id<'a>(node: &'a StyledNode, id: &str) -> Option<&'a StyledNode> {
  if node.node.get_attribute_ref("id") == Some(id) {
    return Some(node);
  }
  node.children.iter().find_map(|child| find_by_id(child, id))
}

#[test]
fn invalid_grid_values_do_not_panic_and_are_ignored() {
  let dom = dom::parse_html(r#"<div id="t"></div>"#).unwrap();
  let stylesheet = parse_stylesheet(
    r#"
      #t {
        display: grid;
        grid-template-columns: [start] 50px [end];
        grid-template-rows: [row] 10px;
        grid-auto-flow: column;
        grid-auto-rows: 20px;
        grid-auto-columns: 30px;
      }

      #t {
        /* Invalid shorthands: missing one side of the slash. */
        grid: auto-flow /;
        grid: / auto-flow;

        /* Invalid track lists / line-name syntax. */
        grid-template-columns: [a;
        grid-template-rows: [];

        /* Unterminated quote in the second row string. */
        grid-template-areas: "a" "b
        ;
      }
    "#,
  )
  .unwrap();

  let styled = apply_styles_with_media(&dom, &stylesheet, &MediaContext::screen(800.0, 600.0));
  let target = find_by_id(&styled, "t").expect("element with id t");

  // Invalid declarations must be ignored per CSS error handling rules.
  assert_eq!(
    target.styles.grid_template_columns,
    vec![GridTrack::Length(Length::px(50.0))]
  );
  assert_eq!(
    target.styles.grid_template_rows,
    vec![GridTrack::Length(Length::px(10.0))]
  );
  assert!(target.styles.grid_template_areas.is_empty());
  assert_eq!(target.styles.grid_auto_flow, GridAutoFlow::Column);
  assert_eq!(
    target.styles.grid_auto_rows.as_ref(),
    &[GridTrack::Length(Length::px(20.0))]
  );
  assert_eq!(
    target.styles.grid_auto_columns.as_ref(),
    &[GridTrack::Length(Length::px(30.0))]
  );
}

