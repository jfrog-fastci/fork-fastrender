use fastrender::tree::fragment_tree::{FragmentContent, FragmentNode};
use fastrender::{FastRender, FontConfig};

fn find_first_line<'a>(node: &'a FragmentNode) -> Option<&'a FragmentNode> {
  if matches!(node.content, FragmentContent::Line { .. }) {
    return Some(node);
  }
  for child in node.children.iter() {
    if let Some(found) = find_first_line(child) {
      return Some(found);
    }
  }
  None
}

fn count_text_fragments(node: &FragmentNode) -> usize {
  let mut count = 0;
  if matches!(node.content, FragmentContent::Text { .. }) {
    count += 1;
  }
  for child in node.children.iter() {
    count += count_text_fragments(child);
  }
  count
}

#[test]
fn text_justify_inter_word_does_not_split_text_into_fragments() {
  // Large number of words to catch any splitting-based justification that would create O(words)
  // fragments. Keep the element narrow so we get multiple lines and can observe justification on a
  // non-last line without needing `text-align-last`.
  let text = format!("{}a", "a ".repeat(600));
  let html = format!(
    "<div style=\"width: 240px; font-family: 'DejaVu Sans', sans-serif; font-size: 16px; line-height: 16px; text-align: justify; text-justify: inter-word;\">{text}</div>"
  );

  let mut renderer = FastRender::builder()
    .font_sources(FontConfig::bundled_only())
    .build()
    .expect("build renderer");

  let dom = renderer.parse_html(&html).expect("parse HTML");
  let fragments = renderer
    .layout_document(&dom, 800, 600)
    .expect("layout document");

  let line = find_first_line(&fragments.root).expect("expected at least one line fragment");

  // Justification should not materialize word boundaries by splitting the text fragment.
  assert_eq!(
    count_text_fragments(line),
    1,
    "expected a single text fragment under the line"
  );

  // The line's contents should expand to fill the line box.
  let mut min_x = f32::INFINITY;
  let mut max_x = f32::NEG_INFINITY;
  for child in &line.children {
    min_x = min_x.min(child.bounds.min_x());
    max_x = max_x.max(child.bounds.max_x());
  }
  let span = max_x - min_x;
  assert!(
    (span - line.bounds.width()).abs() < 0.5,
    "expected justified line to fill its line box span={span:.2} box={:.2}",
    line.bounds.width()
  );
}

