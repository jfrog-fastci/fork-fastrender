use crate::tree::fragment_tree::{FragmentContent, FragmentNode};
use crate::{FastRender, FontConfig};

fn collect_text(node: &FragmentNode, out: &mut String) {
  if let FragmentContent::Text { text, .. } = &node.content {
    out.push_str(text);
  }
  for child in node.children.iter() {
    collect_text(child, out);
  }
}

fn find_first_block_with_line_children<'a>(node: &'a FragmentNode) -> Option<&'a FragmentNode> {
  if matches!(node.content, FragmentContent::Block { .. })
    && node
      .children
      .iter()
      .any(|child| matches!(child.content, FragmentContent::Line { .. }))
  {
    return Some(node);
  }

  for child in node.children.iter() {
    if let Some(found) = find_first_block_with_line_children(child) {
      return Some(found);
    }
  }
  None
}

fn find_first_line_y_with_text(
  node: &FragmentNode,
  origin: (f32, f32),
  needle: &str,
) -> Option<f32> {
  let pos = (origin.0 + node.bounds.x(), origin.1 + node.bounds.y());
  if matches!(node.content, FragmentContent::Line { .. }) {
    let mut text = String::new();
    collect_text(node, &mut text);
    if text.trim() == needle {
      return Some(pos.1);
    }
  }
  for child in node.children.iter() {
    if let Some(y) = find_first_line_y_with_text(child, pos, needle) {
      return Some(y);
    }
  }
  None
}

fn line_texts(html: &str) -> Vec<String> {
  let mut renderer = FastRender::builder()
    .font_sources(FontConfig::bundled_only())
    .build()
    .expect("build renderer");

  let dom = renderer.parse_html(html).expect("parse HTML");
  let fragments = renderer
    .layout_document(&dom, 800, 600)
    .expect("layout document");

  let block = find_first_block_with_line_children(&fragments.root)
    .expect("expected a block fragment with line children");

  block
    .children
    .iter()
    .filter(|child| matches!(child.content, FragmentContent::Line { .. }))
    .map(|line| {
      let mut text = String::new();
      collect_text(line, &mut text);
      text
    })
    .collect()
}

fn line_texts_and_heights(html: &str) -> Vec<(String, f32)> {
  let mut renderer = FastRender::builder()
    .font_sources(FontConfig::bundled_only())
    .build()
    .expect("build renderer");

  let dom = renderer.parse_html(html).expect("parse HTML");
  let fragments = renderer
    .layout_document(&dom, 800, 600)
    .expect("layout document");

  let block = find_first_block_with_line_children(&fragments.root)
    .expect("expected a block fragment with line children");

  block
    .children
    .iter()
    .filter(|child| matches!(child.content, FragmentContent::Line { .. }))
    .map(|line| {
      let mut text = String::new();
      collect_text(line, &mut text);
      (text, line.bounds.height())
    })
    .collect()
}

#[test]
fn br_forces_line_break() {
  let lines = line_texts("<p>hello<br>world</p>");
  assert_eq!(lines, ["hello", "world"]);
}

#[test]
fn br_self_closing_forces_line_break() {
  let lines = line_texts("<p>hello<br/>world</p>");
  assert_eq!(lines, ["hello", "world"]);
}

#[test]
fn br_forces_line_break_under_nowrap() {
  let lines = line_texts("<p style=\"white-space: nowrap\">hello<br>world</p>");
  assert_eq!(lines, ["hello", "world"]);
}

#[test]
fn br_preserves_line_height_for_empty_lines() {
  let html = "<p style=\"font-family: 'DejaVu Sans', sans-serif; font-size: 26px; line-height: 1.2\">hello<br>world<br><br>after blank<br>end</p>";

  let lines = line_texts_and_heights(html);
  let texts: Vec<&str> = lines.iter().map(|(t, _)| t.trim()).collect();
  assert_eq!(texts, ["hello", "world", "", "after blank", "end"]);

  let expected = 26.0 * 1.2;
  for (text, height) in lines {
    assert!(
      (height - expected).abs() < 0.05,
      "line {text:?} height={height:.3} expected={expected:.3}"
    );
  }
}

#[test]
fn nowrap_inline_box_does_not_soft_wrap_across_pseudo_element() {
  // Regression: `white-space: nowrap` must suppress soft wraps across atomic inline boundaries,
  // not just inside text. This catches cases where an inline box is fragmented to fit the line
  // width even though wrapping is disabled (e.g. `span::after { display: inline-block }`).
  let html = r#"
    <style>
      .live { white-space: nowrap }
      .live::after { content: "Live"; display: inline-block }
    </style>
    <p style="width: 40px; font-family: 'DejaVu Sans', sans-serif; font-size: 16px; line-height: 1">
      <span class="live">NASA+</span>
    </p>
  "#;

  let lines = line_texts(html);
  assert_eq!(lines, ["NASA+Live"]);
}

#[test]
fn br_before_block_does_not_create_trailing_empty_line() {
  let lines = line_texts("<div>hello<br><div>block</div></div>");
  assert_eq!(lines, ["hello"]);
}

#[test]
fn multiple_brs_before_block_create_blank_line() {
  // A single `<br>` immediately before a block-level element is treated as redundant (the block
  // already starts on a new line). However, additional `<br>`s must still create empty lines.
  //
  // Real-world markup (e.g. the Hacker News footer) uses `<br><br>` before a `<form>` to insert a
  // blank line between sections; preserve that spacing.
  let lines = line_texts_and_heights("<div>hello<br><br><div>block</div></div>");
  // Ignore any synthetic trailing cursor line (height=0) used for internal layout bookkeeping.
  let visible: Vec<(String, f32)> = lines.into_iter().filter(|(_, h)| *h > 0.01).collect();
  assert_eq!(visible.len(), 2, "expected exactly two visible line boxes");
  let texts: Vec<&str> = visible.iter().map(|(t, _)| t.trim()).collect();
  assert_eq!(texts, ["hello", ""]);
  assert!(
    (visible[1].1 - visible[0].1).abs() < 0.05,
    "blank line height was {} but text line height was {}",
    visible[1].1,
    visible[0].1
  );
}

#[test]
fn leading_br_before_block_still_creates_empty_line() {
  let lines = line_texts("<div><br><div>block</div></div>");
  assert_eq!(lines, ["", ""]);
}

#[test]
fn br_clear_attribute_clears_floats() {
  let html = r#"
    <style>
      body { margin: 0; font-size: 0; line-height: 0; }
      .float { float: right; width: 10px; height: 50px; }
      p { margin: 0; font-family: 'DejaVu Sans', sans-serif; font-size: 16px; line-height: 16px; }
    </style>
    <div class="float"></div>
    <br clear="both">
    <p>after</p>
  "#;

  let mut renderer = FastRender::builder()
    .font_sources(FontConfig::bundled_only())
    .build()
    .expect("build renderer");

  let dom = renderer.parse_html(html).expect("parse HTML");
  let fragments = renderer
    .layout_document(&dom, 200, 200)
    .expect("layout document");

  let y = find_first_line_y_with_text(&fragments.root, (0.0, 0.0), "after")
    .expect("expected a line fragment containing 'after'");
  assert!(
    (y - 50.0).abs() < 0.5,
    "expected text after <br clear=both> to start below the float (y={y})"
  );
}
