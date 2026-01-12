use fastrender::api::FastRender;
use fastrender::style::cascade::StyledNode;
use fastrender::style::media::MediaType;
use fastrender::style::types::TextDecorationLine;

fn styled_tree_for(html: &str) -> StyledNode {
  let mut renderer = FastRender::new().expect("renderer");
  let dom = renderer.parse_html(html).expect("parsed dom");
  renderer
    .layout_document_for_media_intermediates(&dom, 800, 600, MediaType::Screen)
    .expect("laid out")
    .styled_tree
}

fn find_by_id<'a>(node: &'a StyledNode, id: &str) -> Option<&'a StyledNode> {
  if node
    .node
    .get_attribute_ref("id")
    .map(|value| value == id)
    .unwrap_or(false)
  {
    return Some(node);
  }
  for child in node.children.iter() {
    if let Some(found) = find_by_id(child, id) {
      return Some(found);
    }
  }
  None
}

#[test]
fn text_decoration_line_spelling_error_parses() {
  let html = r#"
    <style>
      #target { text-decoration-line: spelling-error; }
    </style>
    <div id="target">test</div>
  "#;

  let styled = styled_tree_for(html);
  let target = find_by_id(&styled, "target").expect("target");
  assert!(target
    .styles
    .text_decoration
    .lines
    .contains(TextDecorationLine::SPELLING_ERROR));
  assert!(!target
    .styles
    .text_decoration
    .lines
    .contains(TextDecorationLine::GRAMMAR_ERROR));
}

#[test]
fn text_decoration_line_grammar_error_parses() {
  let html = r#"
    <style>
      #target { text-decoration-line: grammar-error; }
    </style>
    <div id="target">test</div>
  "#;

  let styled = styled_tree_for(html);
  let target = find_by_id(&styled, "target").expect("target");
  assert!(target
    .styles
    .text_decoration
    .lines
    .contains(TextDecorationLine::GRAMMAR_ERROR));
  assert!(!target
    .styles
    .text_decoration
    .lines
    .contains(TextDecorationLine::SPELLING_ERROR));
}

#[test]
fn text_decoration_line_spelling_error_combines_with_underline() {
  let html = r#"
    <style>
      #target { text-decoration-line: underline spelling-error; }
    </style>
    <div id="target">test</div>
  "#;

  let styled = styled_tree_for(html);
  let target = find_by_id(&styled, "target").expect("target");
  assert!(target
    .styles
    .text_decoration
    .lines
    .contains(TextDecorationLine::UNDERLINE));
  assert!(target
    .styles
    .text_decoration
    .lines
    .contains(TextDecorationLine::SPELLING_ERROR));
}

#[test]
fn text_decoration_shorthand_parses_spelling_error() {
  let html = r#"
    <style>
      #target { text-decoration: spelling-error; }
    </style>
    <div id="target">test</div>
  "#;

  let styled = styled_tree_for(html);
  let target = find_by_id(&styled, "target").expect("target");
  assert!(target
    .styles
    .text_decoration
    .lines
    .contains(TextDecorationLine::SPELLING_ERROR));
}

