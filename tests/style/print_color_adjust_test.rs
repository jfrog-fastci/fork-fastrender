use fastrender::css::parser::parse_stylesheet;
use fastrender::dom;
use fastrender::style::cascade::apply_styles_with_media_target_and_imports;
use fastrender::style::cascade::StyledNode;
use fastrender::style::media::MediaContext;
use fastrender::style::types::PrintColorAdjust;

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

fn styled_tree(html: &str, css: &str) -> StyledNode {
  let dom = dom::parse_html(html).expect("parse html");
  let stylesheet = parse_stylesheet(css).expect("parse stylesheet");
  let media = MediaContext::screen(800.0, 600.0);
  apply_styles_with_media_target_and_imports(
    &dom, &stylesheet, &media, None, None, None, None, None, None,
  )
}

#[test]
fn print_color_adjust_is_inherited() {
  let html = r#"<div id="p"><span id="c">child</span></div>"#;
  let css = r#"#p { print-color-adjust: exact; }"#;
  let styled = styled_tree(html, css);
  let child = find_by_id(&styled, "c").expect("child node");
  assert_eq!(child.styles.print_color_adjust, PrintColorAdjust::Exact);
}

#[test]
fn print_color_adjust_unset_behaves_like_inherit() {
  let html = r#"<div id="p"><span id="c">child</span></div>"#;
  let css = r#"
    #p { print-color-adjust: exact; }
    #c { print-color-adjust: unset; }
  "#;
  let styled = styled_tree(html, css);
  let child = find_by_id(&styled, "c").expect("child node");
  assert_eq!(child.styles.print_color_adjust, PrintColorAdjust::Exact);
}

#[test]
fn color_adjust_shorthand_sets_print_color_adjust() {
  let html = r#"<div id="t">text</div>"#;
  let css = r#"#t { color-adjust: exact; }"#;
  let styled = styled_tree(html, css);
  let node = find_by_id(&styled, "t").expect("node");
  assert_eq!(node.styles.print_color_adjust, PrintColorAdjust::Exact);
}

#[test]
fn webkit_print_color_adjust_alias_sets_print_color_adjust() {
  let html = r#"<div id="t">text</div>"#;
  let css = r#"#t { -webkit-print-color-adjust: exact; }"#;
  let styled = styled_tree(html, css);
  let node = find_by_id(&styled, "t").expect("node");
  assert_eq!(node.styles.print_color_adjust, PrintColorAdjust::Exact);
}

#[test]
fn supports_print_color_adjust_respects_valid_and_invalid_values() {
  let html = r#"<div id="t">text</div>"#;
  let css = r#"
    #t { print-color-adjust: economy; }
    @supports (print-color-adjust: exact) {
      #t { print-color-adjust: exact; }
    }
  "#;
  let styled = styled_tree(html, css);
  let node = find_by_id(&styled, "t").expect("node");
  assert_eq!(node.styles.print_color_adjust, PrintColorAdjust::Exact);

  let css = r#"
    #t { print-color-adjust: economy; }
    @supports (print-color-adjust: bogus) {
      #t { print-color-adjust: exact; }
    }
  "#;
  let styled = styled_tree(html, css);
  let node = find_by_id(&styled, "t").expect("node");
  assert_eq!(node.styles.print_color_adjust, PrintColorAdjust::Economy);
}

#[test]
fn supports_webkit_print_color_adjust_alias_is_true() {
  let html = r#"<div id="t">text</div>"#;
  let css = r#"
    #t { print-color-adjust: economy; }
    @supports (-webkit-print-color-adjust: exact) {
      #t { print-color-adjust: exact; }
    }
  "#;
  let styled = styled_tree(html, css);
  let node = find_by_id(&styled, "t").expect("node");
  assert_eq!(node.styles.print_color_adjust, PrintColorAdjust::Exact);
}
