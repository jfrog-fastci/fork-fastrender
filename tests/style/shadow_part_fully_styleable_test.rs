use fastrender::css::parser::parse_stylesheet;
use fastrender::dom;
use fastrender::style::cascade::apply_styles;
use fastrender::style::cascade::StyledNode;
use fastrender::style::content::ContentValue;
use fastrender::Rgba;

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
fn part_supports_state_pseudo_classes() {
  let html = r#"
    <x-host id="host">
      <template shadowroot="open">
        <button id="button" part="button" data-fastr-hover="true">Button</button>
      </template>
    </x-host>
  "#;
  let dom = dom::parse_html(html).expect("parse html");
  let stylesheet = parse_stylesheet("x-host::part(button):hover { color: rgb(1, 2, 3); }")
    .expect("parse stylesheet");
  let styled = apply_styles(&dom, &stylesheet);

  let button = find_by_id(&styled, "button").expect("part element");
  assert_eq!(button.styles.color, Rgba::rgb(1, 2, 3));
}

#[test]
fn part_supports_tree_abiding_pseudo_elements() {
  let html = r#"
    <x-host id="host">
      <template shadowroot="open">
        <button id="button" part="button">Button</button>
      </template>
    </x-host>
  "#;
  let dom = dom::parse_html(html).expect("parse html");
  let stylesheet = parse_stylesheet(
    r#"x-host::part(button)::before { content: "x"; color: rgb(4, 5, 6); }"#,
  )
  .expect("parse stylesheet");
  let styled = apply_styles(&dom, &stylesheet);

  let button = find_by_id(&styled, "button").expect("part element");
  let before = button.before_styles.as_ref().expect("generated ::before");
  assert_eq!(before.content_value, ContentValue::from_string("x"));
  assert_eq!(before.color, Rgba::rgb(4, 5, 6));
}

