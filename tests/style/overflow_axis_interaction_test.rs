use fastrender::css::parser::parse_stylesheet;
use fastrender::dom;
use fastrender::style::cascade::apply_styles;
use fastrender::style::cascade::StyledNode;
use fastrender::style::types::Overflow;

fn find_by_id<'a>(node: &'a StyledNode, id: &str) -> Option<&'a StyledNode> {
  if node
    .node
    .get_attribute_ref("id")
    .is_some_and(|value| value.eq_ignore_ascii_case(id))
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
fn overflow_axes_are_normalized_after_cascade() {
  let html = r#"
    <div id="ov-visible"></div>
    <div id="ov-hidden"></div>
    <div id="ox-visible-oy-hidden"></div>
    <div id="ox-visible-oy-scroll"></div>
    <div id="ox-visible-oy-auto"></div>
    <div id="ox-visible-oy-clip"></div>
    <div id="ox-clip-oy-visible"></div>
    <div id="ox-hidden-oy-visible"></div>
  "#;

  let css = r#"
    #ov-visible { overflow: visible; }
    #ov-hidden { overflow: hidden; }
    #ox-visible-oy-hidden { overflow-x: visible; overflow-y: hidden; }
    #ox-visible-oy-scroll { overflow-x: visible; overflow-y: scroll; }
    #ox-visible-oy-auto { overflow-x: visible; overflow-y: auto; }
    #ox-visible-oy-clip { overflow-x: visible; overflow-y: clip; }
    #ox-clip-oy-visible { overflow-x: clip; overflow-y: visible; }
    #ox-hidden-oy-visible { overflow-x: hidden; overflow-y: visible; }
  "#;

  let dom = dom::parse_html(html).expect("parse html");
  let stylesheet = parse_stylesheet(css).expect("parse stylesheet");
  let styled = apply_styles(&dom, &stylesheet);

  let visible = find_by_id(&styled, "ov-visible").expect("ov-visible");
  assert_eq!(visible.styles.overflow_x, Overflow::Visible);
  assert_eq!(visible.styles.overflow_y, Overflow::Visible);

  let hidden = find_by_id(&styled, "ov-hidden").expect("ov-hidden");
  assert_eq!(hidden.styles.overflow_x, Overflow::Hidden);
  assert_eq!(hidden.styles.overflow_y, Overflow::Hidden);

  let vis_hidden = find_by_id(&styled, "ox-visible-oy-hidden").expect("ox-visible-oy-hidden");
  assert_eq!(vis_hidden.styles.overflow_x, Overflow::Auto);
  assert_eq!(vis_hidden.styles.overflow_y, Overflow::Hidden);

  let vis_scroll = find_by_id(&styled, "ox-visible-oy-scroll").expect("ox-visible-oy-scroll");
  assert_eq!(vis_scroll.styles.overflow_x, Overflow::Auto);
  assert_eq!(vis_scroll.styles.overflow_y, Overflow::Scroll);

  let vis_auto = find_by_id(&styled, "ox-visible-oy-auto").expect("ox-visible-oy-auto");
  assert_eq!(vis_auto.styles.overflow_x, Overflow::Auto);
  assert_eq!(vis_auto.styles.overflow_y, Overflow::Auto);

  let vis_clip = find_by_id(&styled, "ox-visible-oy-clip").expect("ox-visible-oy-clip");
  assert_eq!(vis_clip.styles.overflow_x, Overflow::Visible);
  assert_eq!(vis_clip.styles.overflow_y, Overflow::Clip);

  let clip_vis = find_by_id(&styled, "ox-clip-oy-visible").expect("ox-clip-oy-visible");
  assert_eq!(clip_vis.styles.overflow_x, Overflow::Clip);
  assert_eq!(clip_vis.styles.overflow_y, Overflow::Visible);

  let hidden_vis = find_by_id(&styled, "ox-hidden-oy-visible").expect("ox-hidden-oy-visible");
  assert_eq!(hidden_vis.styles.overflow_x, Overflow::Hidden);
  assert_eq!(hidden_vis.styles.overflow_y, Overflow::Auto);
}

