use fastrender::dom::{DomNode, DomNodeType, HTML_NAMESPACE};
use fastrender::style::defaults::get_default_styles_for_element;

fn element(tag: &str) -> DomNode {
  DomNode {
    node_type: DomNodeType::Element {
      tag_name: tag.to_string(),
      namespace: HTML_NAMESPACE.to_string(),
      attributes: Vec::new(),
    },
    children: Vec::new(),
  }
}

#[test]
fn legend_defaults_to_shrink_to_fit_inline_size() {
  let style = get_default_styles_for_element(&element("legend"));
  assert!(
    style.shrink_to_fit_inline_size,
    "<legend> needs shrink-to-fit sizing even before UA CSS"
  );
}
