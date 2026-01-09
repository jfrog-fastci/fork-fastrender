use fastrender::css::parser::parse_stylesheet;
use fastrender::dom;
use fastrender::dom::{DomNode, DomNodeType, HTML_NAMESPACE};
use fastrender::style::cascade::{apply_styles, StyledNode};
use fastrender::style::color::Rgba;
use fastrender::style::display::Display;

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
fn revert_layer_vs_revert_across_multiple_layers() {
  let dom = dom::parse_html(r#"<div id="t1"></div><div id="t2"></div>"#).expect("parse html");
  let css = r#"
    @layer base {
      #t1, #t2 { display: inline; }
    }
    @layer theme {
      #t1, #t2 { display: inline-block; }
      #t1 { display: revert-layer; }
      #t2 { display: revert; }
    }
  "#;
  let sheet = parse_stylesheet(css).expect("parse stylesheet");
  let styled = apply_styles(&dom, &sheet);

  let t1 = find_by_id(&styled, "t1").expect("t1");
  assert_eq!(t1.styles.display, Display::Inline);

  let t2 = find_by_id(&styled, "t2").expect("t2");
  // `revert` rolls back the author origin entirely, so `div` returns to UA `display: block`.
  assert_eq!(t2.styles.display, Display::Block);
}

#[test]
fn important_revert_layer_ignores_normal_declarations_in_the_same_layer() {
  let dom = dom::parse_html(r#"<div id="t"></div>"#).expect("parse html");
  let css = r#"
    @layer base { #t { display: inline; } }
    @layer theme { #t { display: inline-block; } }
    @layer theme { #t { display: revert-layer !important; } }
  "#;
  let sheet = parse_stylesheet(css).expect("parse stylesheet");
  let styled = apply_styles(&dom, &sheet);

  let t = find_by_id(&styled, "t").expect("t");
  assert_eq!(t.styles.display, Display::Inline);
}

#[test]
fn unset_on_inherited_and_non_inherited_properties() {
  let dom = dom::parse_html(r#"<div id="parent"><div id="child"></div></div>"#).expect("parse html");
  let css = r#"
    #parent { color: rgb(10, 20, 30); }
    #child { color: rgb(1, 2, 3); display: inline-block; }
    #child { color: unset; display: unset; }
  "#;
  let sheet = parse_stylesheet(css).expect("parse stylesheet");
  let styled = apply_styles(&dom, &sheet);

  let child = find_by_id(&styled, "child").expect("child");
  assert_eq!(child.styles.color, Rgba::rgb(10, 20, 30));
  // `display` is not inherited, so `unset` behaves like `initial` (inline), not the UA default.
  assert_eq!(child.styles.display, Display::Inline);
}

#[test]
fn scope_root_matches_self_as_well_as_descendants() {
  // Construct a DOM tree where the scope root has no element parent.
  let dom = DomNode {
    node_type: DomNodeType::Element {
      tag_name: "div".to_string(),
      namespace: HTML_NAMESPACE.to_string(),
      attributes: vec![
        ("id".to_string(), "scope".to_string()),
        ("class".to_string(), "scope".to_string()),
      ],
    },
    children: vec![DomNode {
      node_type: DomNodeType::Element {
        tag_name: "p".to_string(),
        namespace: HTML_NAMESPACE.to_string(),
        attributes: vec![
          ("id".to_string(), "inside".to_string()),
          ("class".to_string(), "target".to_string()),
        ],
      },
      children: vec![],
    }],
  };

  let css = r#"
    @scope (.scope) {
      :scope { display: inline-block; }
      .target { display: inline; }
    }
  "#;
  let sheet = parse_stylesheet(css).expect("parse stylesheet");
  let styled = apply_styles(&dom, &sheet);

  let scope = find_by_id(&styled, "scope").expect("scope root");
  assert_eq!(scope.styles.display, Display::InlineBlock);
  let inside = find_by_id(&styled, "inside").expect("inside");
  assert_eq!(inside.styles.display, Display::Inline);
}

#[test]
fn shadow_root_tree_scope_precedence_for_same_selector() {
  // Same selector in two tree contexts (document vs shadow). Tree-scope precedence determines the
  // winner rather than selector specificity.
  let html = r#"
    <x-outer id="outer">
      <template shadowroot="open">
        <style>
          x-inner::part(label) { color: rgb(4, 5, 6); }
        </style>
        <x-inner id="inner">
          <template shadowroot="open">
            <span id="part" part="label">Inner</span>
          </template>
        </x-inner>
      </template>
    </x-outer>
  "#;

  let dom = dom::parse_html(html).expect("parse html");
  let document_sheet =
    parse_stylesheet("x-inner::part(label) { color: rgb(1, 2, 3); }").expect("parse stylesheet");
  let styled = apply_styles(&dom, &document_sheet);

  let part = find_by_id(&styled, "part").expect("part element");
  assert_eq!(part.styles.color, Rgba::rgb(4, 5, 6));
}
