use fastrender::css::types::StyleSheet;
use fastrender::dom;
use fastrender::style::cascade::apply_styles;
use fastrender::tree::box_generation::generate_box_tree;
use fastrender::tree::box_tree::{BoxNode, BoxType, GeneratedPseudoElement, ReplacedType};

fn find_styled_node_id_by_element_id(styled: &fastrender::style::cascade::StyledNode, id: &str) -> Option<usize> {
  if styled.node.get_attribute_ref("id") == Some(id) {
    return Some(styled.node_id);
  }
  styled
    .children
    .iter()
    .find_map(|child| find_styled_node_id_by_element_id(child, id))
}

fn find_box_by_styled_id<'a>(node: &'a BoxNode, styled_id: usize) -> Option<&'a BoxNode> {
  if node.styled_node_id == Some(styled_id) {
    return Some(node);
  }
  node.children.iter().find_map(|child| find_box_by_styled_id(child, styled_id))
}

fn has_descendant_with_styled_id(node: &BoxNode, styled_id: usize) -> bool {
  if node.styled_node_id == Some(styled_id) {
    return true;
  }
  node
    .children
    .iter()
    .any(|child| has_descendant_with_styled_id(child, styled_id))
}

fn count_form_control_replacements(node: &BoxNode) -> usize {
  let mut count = 0usize;
  if let BoxType::Replaced(repl) = &node.box_type {
    if matches!(repl.replaced_type, ReplacedType::FormControl(_)) {
      count += 1;
    }
  }
  for child in node.children.iter() {
    count += count_form_control_replacements(child);
  }
  count
}

fn has_generated_pseudo(node: &BoxNode, pseudo: GeneratedPseudoElement) -> bool {
  if node.generated_pseudo == Some(pseudo) {
    return true;
  }
  node.children.iter().any(|child| has_generated_pseudo(child, pseudo))
}

#[test]
fn button_appearance_none_preserves_dom_children() {
  let html = "<html><body><button id=\"btn\" style=\"appearance:none\"><span id=\"inner\">Hello</span></button></body></html>";
  let dom = dom::parse_html(html).expect("parse html");
  let styled = apply_styles(&dom, &StyleSheet::new());

  let btn_id = find_styled_node_id_by_element_id(&styled, "btn").expect("button styled node id");
  let span_id = find_styled_node_id_by_element_id(&styled, "inner").expect("span styled node id");

  let tree = generate_box_tree(&styled).expect("box tree");
  assert_eq!(
    count_form_control_replacements(&tree.root),
    0,
    "appearance:none buttons should not create replaced form controls"
  );

  let btn_box = find_box_by_styled_id(&tree.root, btn_id).expect("button box");
  assert!(
    has_descendant_with_styled_id(btn_box, span_id),
    "expected button descendants to generate boxes when appearance:none"
  );
  assert!(
    btn_box.text().is_none(),
    "button box should not collapse children into a synthetic label text node"
  );
}

#[test]
fn range_appearance_none_generates_slider_track_and_thumb_boxes() {
  let html = "<html><body><input id=\"slider\" type=\"range\" style=\"appearance:none\" /></body></html>";
  let dom = dom::parse_html(html).expect("parse html");
  let styled = apply_styles(&dom, &StyleSheet::new());

  let slider_id = find_styled_node_id_by_element_id(&styled, "slider").expect("slider styled node id");
  let tree = generate_box_tree(&styled).expect("box tree");

  assert_eq!(
    count_form_control_replacements(&tree.root),
    0,
    "appearance:none range inputs should not create replaced form controls"
  );

  let slider_box = find_box_by_styled_id(&tree.root, slider_id).expect("slider box");
  assert!(
    has_generated_pseudo(slider_box, GeneratedPseudoElement::SliderTrack),
    "expected range track pseudo-element box to be generated"
  );
  assert!(
    has_generated_pseudo(slider_box, GeneratedPseudoElement::SliderThumb),
    "expected range thumb pseudo-element box to be generated"
  );
}

#[test]
fn file_input_appearance_none_generates_file_selector_button_box() {
  let html = "<html><body><input id=\"file\" type=\"file\" style=\"appearance:none\" /></body></html>";
  let dom = dom::parse_html(html).expect("parse html");
  let styled = apply_styles(&dom, &StyleSheet::new());

  let file_id = find_styled_node_id_by_element_id(&styled, "file").expect("file styled node id");
  let tree = generate_box_tree(&styled).expect("box tree");

  assert_eq!(
    count_form_control_replacements(&tree.root),
    0,
    "appearance:none file inputs should not create replaced form controls"
  );

  let file_box = find_box_by_styled_id(&tree.root, file_id).expect("file input box");
  assert!(
    has_generated_pseudo(file_box, GeneratedPseudoElement::FileSelectorButton),
    "expected file-selector-button pseudo-element box to be generated"
  );
}

