use fastrender::css::parser::parse_stylesheet;
use fastrender::dom;
use fastrender::style::cascade::apply_styles_with_media;
use fastrender::style::cascade::StyledNode;
use fastrender::style::media::MediaContext;
use fastrender::tree::box_generation::generate_box_tree;
use fastrender::tree::box_tree::BoxNode;
use fastrender::tree::box_tree::BoxType;
use fastrender::tree::box_tree::GeneratedPseudoElement;

fn find_by_id<'a>(node: &'a StyledNode, id: &str) -> Option<&'a StyledNode> {
  if node.node.get_attribute_ref("id").is_some_and(|value| value == id) {
    return Some(node);
  }
  for child in node.children.iter() {
    if let Some(found) = find_by_id(child, id) {
      return Some(found);
    }
  }
  None
}

fn collect_pseudo_text(
  node: &BoxNode,
  styled_node_id: usize,
  pseudo: GeneratedPseudoElement,
  out: &mut String,
) {
  if node.styled_node_id == Some(styled_node_id) && node.generated_pseudo == Some(pseudo) {
    if let BoxType::Text(text) = &node.box_type {
      out.push_str(&text.text);
    }
  }
  for child in node.children.iter() {
    collect_pseudo_text(child, styled_node_id, pseudo, out);
  }
}

fn generated_before_text(tree: &fastrender::tree::box_tree::BoxTree, styled_node_id: usize) -> String {
  let mut out = String::new();
  collect_pseudo_text(
    &tree.root,
    styled_node_id,
    GeneratedPseudoElement::Before,
    &mut out,
  );
  out
}

fn run_quote_scoping_case(container_extra_style: &str) -> (String, String, String) {
  let dom = dom::parse_html(
    r#"
      <div id="root">
        <div id="a"></div>
        <div id="container">
          <div id="b"></div>
        </div>
        <div id="c"></div>
      </div>
    "#,
  )
  .unwrap();

  let css = format!(
    r#"
      #root {{ quotes: "<" ">" "[" "]"; }}
      #a::before {{ content: open-quote; }}
      #b::before {{ content: open-quote; }}
      #c::before {{ content: close-quote; }}
      #container {{ {container_extra_style} }}
    "#
  );

  let stylesheet = parse_stylesheet(&css).unwrap();
  let styled = apply_styles_with_media(&dom, &stylesheet, &MediaContext::screen(800.0, 600.0));

  let a = find_by_id(&styled, "a").expect("expected #a node");
  let b = find_by_id(&styled, "b").expect("expected #b node");
  let c = find_by_id(&styled, "c").expect("expected #c node");

  let tree = generate_box_tree(&styled).unwrap();
  (
    generated_before_text(&tree, a.node_id),
    generated_before_text(&tree, b.node_id),
    generated_before_text(&tree, c.node_id),
  )
}

#[test]
fn style_containment_quotes_propagate_without_containment() {
  let (a, b, c) = run_quote_scoping_case("");
  assert_eq!(a, "<");
  assert_eq!(b, "[");
  assert_eq!(c, "]");
}

#[test]
fn style_containment_scopes_quote_depth_at_boundary() {
  let (a, b, c) = run_quote_scoping_case("contain: style;");
  assert_eq!(a, "<");
  // The quote depth inside the subtree starts from its context (so the nested quote is used)...
  assert_eq!(b, "[");
  // ...but changes inside the subtree do not affect the quote depth outside.
  assert_eq!(c, ">");
}

