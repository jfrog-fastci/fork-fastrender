use fastrender::css::parser::parse_stylesheet;
use fastrender::dom::{self, DomNode, DomNodeType};
use fastrender::style::cascade::{apply_styles_with_media, StyledNode};
use fastrender::style::media::MediaContext;
use fastrender::tree::box_generation::generate_box_tree;
use fastrender::tree::box_tree::{BoxNode, BoxType, FormControlKind, ReplacedType};
use fastrender::Length;

fn find_by_tag<'a>(node: &'a StyledNode, tag: &str) -> Option<&'a StyledNode> {
  if let Some(name) = node.node.tag_name() {
    if name.eq_ignore_ascii_case(tag) {
      return Some(node);
    }
  }
  for child in node.children.iter() {
    if let Some(found) = find_by_tag(child, tag) {
      return Some(found);
    }
  }
  None
}

fn find_textarea_value<'a>(node: &'a BoxNode) -> Option<&'a str> {
  if let BoxType::Replaced(replaced) = &node.box_type {
    if let ReplacedType::FormControl(control) = &replaced.replaced_type {
      if let FormControlKind::TextArea { value, .. } = &control.control {
        return Some(value.as_str());
      }
    }
  }
  for child in node.children.iter() {
    if let Some(value) = find_textarea_value(child) {
      return Some(value);
    }
  }
  None
}

fn find_first_element_mut<'a>(node: &'a mut DomNode, tag: &str) -> Option<&'a mut DomNode> {
  if node.tag_name().is_some_and(|t| t.eq_ignore_ascii_case(tag)) {
    return Some(node);
  }
  for child in node.children.iter_mut() {
    if let Some(found) = find_first_element_mut(child, tag) {
      return Some(found);
    }
  }
  None
}

fn set_attribute(node: &mut DomNode, name: &str, value: &str) {
  let attrs = match &mut node.node_type {
    DomNodeType::Element { attributes, .. } | DomNodeType::Slot { attributes, .. } => attributes,
    _ => return,
  };

  if let Some((_, existing)) = attrs.iter_mut().find(|(k, _)| k.eq_ignore_ascii_case(name)) {
    existing.clear();
    existing.push_str(value);
    return;
  }
  attrs.push((name.to_string(), value.to_string()));
}

#[test]
fn textarea_default_value_strips_single_leading_newline_when_no_data_fastr_value() {
  let html = "<textarea>\nhello</textarea>";
  let dom = dom::parse_html(html).expect("parse html");

  let sheet = parse_stylesheet("").expect("parse stylesheet");
  let styled = apply_styles_with_media(&dom, &sheet, &MediaContext::screen(800.0, 600.0));
  let box_tree = generate_box_tree(&styled).expect("box tree");

  let value = find_textarea_value(&box_tree.root).expect("textarea control value");
  assert_eq!(value, "hello");
}

#[test]
fn textarea_runtime_value_preserves_leading_newline_and_drives_box_generation() {
  let css = r#"
    textarea { border-top-width: 1px; border-top-style: solid; }
    textarea:placeholder-shown { border-top-width: 2px; }
  "#;
  let sheet = parse_stylesheet(css).expect("parse stylesheet");

  // Empty textarea shows placeholder, so `:placeholder-shown` should match.
  let dom_empty = dom::parse_html(r#"<textarea placeholder="p"></textarea>"#).expect("parse html");
  let styled_empty =
    apply_styles_with_media(&dom_empty, &sheet, &MediaContext::screen(800.0, 600.0));
  let textarea_empty = find_by_tag(&styled_empty, "textarea").expect("textarea present");
  assert_eq!(textarea_empty.styles.border_top_width, Length::px(2.0));

  // Once the user has interacted with the control, we persist the value in `data-fastr-value`.
  // Leading newlines are part of the runtime value and must not be stripped.
  let mut dom = dom::parse_html(r#"<textarea placeholder="p">x</textarea>"#).expect("parse html");
  let textarea = find_first_element_mut(&mut dom, "textarea").expect("textarea present");
  set_attribute(textarea, "data-fastr-value", "\nabc");

  let styled = apply_styles_with_media(&dom, &sheet, &MediaContext::screen(800.0, 600.0));
  let textarea_styled = find_by_tag(&styled, "textarea").expect("textarea present");
  // Non-empty runtime value => placeholder not shown.
  assert_eq!(textarea_styled.styles.border_top_width, Length::px(1.0));

  let box_tree = generate_box_tree(&styled).expect("box tree");
  let value = find_textarea_value(&box_tree.root).expect("textarea control value");
  assert_eq!(value, "\nabc");
}
