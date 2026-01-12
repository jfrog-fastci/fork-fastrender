use fastrender::css::parser::parse_stylesheet;
use fastrender::dom;
use fastrender::style::cascade::apply_styles;
use fastrender::style::cascade::StyledNode;
use fastrender::Display;

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
fn display_parse_accepts_multi_keyword_syntax() {
  assert_eq!(
    Display::parse("inline flex").expect("inline flex"),
    Display::InlineFlex
  );
  assert_eq!(
    Display::parse("flex inline").expect("flex inline"),
    Display::InlineFlex
  );
  assert_eq!(
    Display::parse("inline/*comment*/flex").expect("comment whitespace"),
    Display::InlineFlex
  );
  assert_eq!(
    Display::parse("block flow-root").expect("block flow-root"),
    Display::FlowRoot
  );
  assert_eq!(
    Display::parse("flow-root inline").expect("flow-root inline"),
    Display::InlineBlock
  );
  assert_eq!(Display::parse("flow").expect("flow"), Display::Block);
}

#[test]
fn display_property_accepts_multi_keyword_syntax() {
  let dom = dom::parse_html(
    r#"
      <div id="inline_flex"></div>
      <div id="flex_inline"></div>
      <div id="inline_comment"></div>
      <div id="inline_flow_root"></div>
      <div id="block_flow_root"></div>
    "#,
  )
  .expect("parse html");

  let stylesheet = parse_stylesheet(
    r#"
      #inline_flex { display: inline flex; }
      #flex_inline { display: flex inline; }
      #inline_comment { display: inline/*comment*/flex; }
      #inline_flow_root { display: inline flow-root; }
      #block_flow_root { display: block flow-root; }
    "#,
  )
  .expect("stylesheet");

  let styled = apply_styles(&dom, &stylesheet);

  let inline_flex = find_by_id(&styled, "inline_flex").expect("inline_flex element");
  assert_eq!(inline_flex.styles.display, Display::InlineFlex);

  let flex_inline = find_by_id(&styled, "flex_inline").expect("flex_inline element");
  assert_eq!(flex_inline.styles.display, Display::InlineFlex);

  let inline_comment = find_by_id(&styled, "inline_comment").expect("inline_comment element");
  assert_eq!(inline_comment.styles.display, Display::InlineFlex);

  let inline_flow_root = find_by_id(&styled, "inline_flow_root").expect("inline_flow_root element");
  assert_eq!(inline_flow_root.styles.display, Display::InlineBlock);

  let block_flow_root = find_by_id(&styled, "block_flow_root").expect("block_flow_root element");
  assert_eq!(block_flow_root.styles.display, Display::FlowRoot);
}

#[test]
fn supports_display_multi_keyword_values() {
  let dom = dom::parse_html(r#"<div id="target"></div>"#).expect("parse html");
  let stylesheet = parse_stylesheet(
    r#"
      #target { z-index: 1; }
      @supports (display: inline flex) {
        #target { z-index: 2; }
      }
    "#,
  )
  .expect("stylesheet");

  let styled = apply_styles(&dom, &stylesheet);
  let target = find_by_id(&styled, "target").expect("target element");
  assert_eq!(target.styles.z_index, Some(2));
}

