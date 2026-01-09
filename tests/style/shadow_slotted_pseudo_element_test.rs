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
fn slotted_supports_tree_abiding_pseudo_elements() {
  let html = r#"
    <div id="host">
      <template shadowroot="open">
        <style>
          slot::slotted(.target)::before { content: "x"; color: rgb(7, 8, 9); }
        </style>
        <slot></slot>
      </template>
      <span id="light" class="target">Light</span>
    </div>
  "#;
  let dom = dom::parse_html(html).expect("parse html");
  let stylesheet = parse_stylesheet("").expect("parse stylesheet");
  let styled = apply_styles(&dom, &stylesheet);

  let light = find_by_id(&styled, "light").expect("slotted element");
  let before = light.before_styles.as_ref().expect("generated ::before");
  assert_eq!(before.content_value, ContentValue::from_string("x"));
  assert_eq!(before.color, Rgba::rgb(7, 8, 9));
}

