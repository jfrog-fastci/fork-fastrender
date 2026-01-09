use fastrender::css::parser::parse_stylesheet;
use fastrender::dom;
use fastrender::style::cascade::{apply_styles_with_media, StyledNode};
use fastrender::style::media::MediaContext;
use fastrender::tree::box_generation::generate_box_tree;
use fastrender::tree::box_tree::{BoxNode, BoxType, GeneratedPseudoElement};

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
  if let Some(body) = node.footnote_body.as_deref() {
    collect_pseudo_text(body, styled_node_id, pseudo, out);
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

fn run_counter_leakage_case(container_extra_style: &str) -> String {
  let dom = dom::parse_html(
    r#"
      <div id="root">
        <div id="container">
          <div id="a"></div>
        </div>
        <div id="b"></div>
      </div>
    "#,
  )
  .unwrap();

  let css = format!(
    r#"
      #root {{ counter-reset: c; }}
      #a {{ counter-increment: c; }}
      #b::before {{ content: counter(c); }}
      #container {{ {container_extra_style} }}
    "#
  );

  let stylesheet = parse_stylesheet(&css).unwrap();
  let styled = apply_styles_with_media(&dom, &stylesheet, &MediaContext::screen(800.0, 600.0));

  let b_node = find_by_id(&styled, "b").expect("expected #b node");
  let tree = generate_box_tree(&styled).unwrap();

  generated_before_text(&tree, b_node.node_id)
}

#[test]
fn counter_leakage_without_style_containment_increments_globally() {
  assert_eq!(run_counter_leakage_case(""), "1");
}

#[test]
fn counter_leakage_is_blocked_by_contain_style() {
  assert_eq!(run_counter_leakage_case("contain: style;"), "0");
}

#[test]
fn counter_leakage_is_blocked_by_content_visibility_implied_style_containment() {
  assert_eq!(run_counter_leakage_case("content-visibility: hidden;"), "0");
}

