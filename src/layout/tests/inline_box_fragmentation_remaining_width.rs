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

fn collect_line_texts(node: &FragmentNode, out: &mut Vec<String>) {
  if matches!(node.content, FragmentContent::Line { .. }) {
    let mut text = String::new();
    collect_text(node, &mut text);
    out.push(text);
  }
  for child in node.children.iter() {
    collect_line_texts(child, out);
  }
}

#[test]
fn inline_boxes_can_fragment_without_starting_on_new_line() {
  let mut renderer = FastRender::builder()
    .font_sources(FontConfig::bundled_only())
    .build()
    .expect("build renderer");

  let dom = renderer
    .parse_html(
      r#"<!doctype html>
        <style>
          html, body { margin: 0; padding: 0; }
          #box { width: 120px; font: 16px/1 monospace; }
          span { background: yellow; }
        </style>
        <div id="box">Hello <span>world world</span></div>
      "#,
    )
    .expect("parse HTML");

  let fragments = renderer
    .layout_document(&dom, 200, 200)
    .expect("layout document");

  let mut lines = Vec::new();
  collect_line_texts(&fragments.root, &mut lines);
  let trimmed: Vec<&str> = lines.iter().map(|line| line.trim()).collect();

  assert_eq!(trimmed, ["Hello world", "world"]);
}

#[test]
fn inline_boxes_do_not_use_emergency_breaks_mid_line() {
  let mut renderer = FastRender::builder()
    .font_sources(FontConfig::bundled_only())
    .build()
    .expect("build renderer");

  let dom = renderer
    .parse_html(
      r#"<!doctype html>
        <style>
          html, body { margin: 0; padding: 0; }
          #box { width: 120px; font: 16px/1 monospace; overflow-wrap: break-word; }
          span { background: yellow; }
        </style>
        <div id="box">Hello <span>transform</span></div>
      "#,
    )
    .expect("parse HTML");

  let fragments = renderer
    .layout_document(&dom, 200, 200)
    .expect("layout document");

  let mut lines = Vec::new();
  collect_line_texts(&fragments.root, &mut lines);
  let trimmed: Vec<&str> = lines.iter().map(|line| line.trim()).collect();

  // Even with `overflow-wrap: break-word`, a word inside an inline box should not be split mid-word
  // just to use leftover space at the end of a line; it should move to the next line instead.
  assert_eq!(trimmed, ["Hello", "transform"]);
}

#[test]
fn inline_boxes_do_not_use_emergency_breaks_mid_line_on_utf8_boundaries() {
  let mut renderer = FastRender::builder()
    .font_sources(FontConfig::bundled_only())
    .build()
    .expect("build renderer");

  let dom = renderer
    .parse_html(
      r#"<!doctype html>
        <style>
          html, body { margin: 0; padding: 0; }
          #box { width: 140px; font: 16px/1 monospace; overflow-wrap: break-word; }
          span { background: yellow; }
        </style>
        <div id="box">Hello <span>trânsformable</span></div>
      "#,
    )
    .expect("parse HTML");

  let fragments = renderer
    .layout_document(&dom, 200, 200)
    .expect("layout document");

  let mut lines = Vec::new();
  collect_line_texts(&fragments.root, &mut lines);
  let trimmed: Vec<&str> = lines.iter().map(|line| line.trim()).collect();

  // The emergency-break suppression heuristic should be robust across UTF-8 character boundaries.
  assert_eq!(trimmed, ["Hello", "trânsformable"]);
}

#[test]
fn inline_boxes_do_not_spuriously_break_before_leading_collapsible_space() {
  let mut renderer = FastRender::builder()
    .font_sources(FontConfig::bundled_only())
    .build()
    .expect("build renderer");

  let dom = renderer
    .parse_html(
      r#"<!doctype html>
        <style>
          html, body { margin: 0; padding: 0; }
          #box { width: 300px; font: 16px/1 sans-serif; }
          code { font-family: monospace; }
        </style>
        <div id="box">
          The <strong><code>text-combine-upright</code></strong> CSS property sets the combination of
          characters into the space of a single character.
        </div>
      "#,
    )
    .expect("parse HTML");

  let fragments = renderer
    .layout_document(&dom, 400, 200)
    .expect("layout document");

  let mut lines = Vec::new();
  collect_line_texts(&fragments.root, &mut lines);
  let trimmed: Vec<String> = lines
    .into_iter()
    .map(|line| line.trim().to_string())
    .collect();

  assert!(
    trimmed
      .first()
      .is_some_and(|line| line.contains("text-combine-upright CSS")),
    "expected first line to contain `text-combine-upright CSS`; got {trimmed:?}"
  );
}
