use fastrender::css::parser::parse_stylesheet;
use fastrender::dom;
use fastrender::style::cascade::apply_styles_with_media;
use fastrender::style::cascade::StyledNode;
use fastrender::style::media::MediaContext;
use fastrender::style::types::TransitionTimingFunction;

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

fn styled_div(html: &str) -> StyledNode {
  let dom = dom::parse_html(html).unwrap();
  let stylesheet = parse_stylesheet("").unwrap();
  let styled = apply_styles_with_media(&dom, &stylesheet, &MediaContext::screen(800.0, 600.0));
  find_first(&styled, "div").expect("div").clone()
}

fn assert_single_linear_function(funcs: &[TransitionTimingFunction]) {
  assert_eq!(funcs.len(), 1);
  match &funcs[0] {
    TransitionTimingFunction::LinearFunction(stops) => {
      assert!(stops.len() >= 2);
    }
    other => panic!("expected linear() function, got {other:?}"),
  }
}

#[test]
fn transition_timing_function_invalid_linear_stop_list_does_not_panic_or_override() {
  let node = styled_div(
    r#"<div style="transition-timing-function: linear(0, 1); transition-timing-function: linear(0, 1, , 0.5);"></div>"#,
  );

  // The invalid declaration should be ignored, leaving the earlier valid `linear()` function.
  assert_single_linear_function(&node.styles.transition_timing_functions);
}

#[test]
fn transition_timing_function_linear_missing_outputs_does_not_panic_or_override() {
  let node = styled_div(
    r#"<div style="transition-timing-function: linear(0, 1); transition-timing-function: linear(0 0, 1);"></div>"#,
  );

  // The invalid declaration should be ignored, leaving the earlier valid `linear()` function.
  assert_single_linear_function(&node.styles.transition_timing_functions);
}

