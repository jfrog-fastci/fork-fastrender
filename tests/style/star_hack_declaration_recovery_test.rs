use fastrender::css::parser::parse_stylesheet;
use fastrender::dom::{DomNode, DomNodeType, HTML_NAMESPACE};
use fastrender::style::cascade::apply_styles_with_media;
use fastrender::style::color::Rgba;
use fastrender::style::media::MediaContext;

#[test]
fn star_hack_declaration_does_not_abort_style_block_parsing() {
  // Some legacy stylesheets (including python.org) ship invalid "star-hack" declarations like
  // `*font-size: small;`. These must be ignored without preventing subsequent valid declarations
  // from being parsed.
  let css = "div { *color: red; color: blue; }";
  let sheet = parse_stylesheet(css).expect("stylesheet");

  let dom = DomNode {
    node_type: DomNodeType::Element {
      tag_name: "div".into(),
      namespace: HTML_NAMESPACE.to_string(),
      attributes: vec![],
    },
    children: vec![],
  };

  let styled = apply_styles_with_media(&dom, &sheet, &MediaContext::default());
  assert_eq!(styled.styles.color, Rgba::BLUE);
}

