use fastrender::geometry::Point;
use fastrender::tree::fragment_tree::{FragmentContent, FragmentNode};
use fastrender::Rgba;
use fastrender::{FastRender, FontConfig};

const EPS: f32 = 0.01;

fn layout_html(
  renderer: &mut FastRender,
  html: &str,
) -> fastrender::tree::fragment_tree::FragmentTree {
  let dom = renderer.parse_html(html).expect("parse HTML");
  renderer.layout_document(&dom, 800, 600).expect("layout")
}

fn collect_text(node: &FragmentNode, out: &mut String) {
  if let FragmentContent::Text { text, .. } = &node.content {
    out.push_str(text);
  }
  for child in node.children.iter() {
    collect_text(child, out);
  }
}

fn collect_text_fragments(node: &FragmentNode, offset: Point, out: &mut Vec<(String, f32, f32)>) {
  let abs = Point::new(offset.x + node.bounds.x(), offset.y + node.bounds.y());
  if let FragmentContent::Text { text, .. } = &node.content {
    out.push((text.to_string(), abs.x, node.bounds.width()));
  }
  for child in node.children.iter() {
    collect_text_fragments(child, abs, out);
  }
}

fn collect_background_fragments(
  node: &FragmentNode,
  offset: Point,
  color: Rgba,
  out: &mut Vec<(f32, f32, f32, f32)>,
) {
  let abs = Point::new(offset.x + node.bounds.x(), offset.y + node.bounds.y());
  if let Some(style) = node.style.as_ref() {
    if style.background_color == color {
      out.push((abs.x, abs.y, node.bounds.width(), node.bounds.height()));
    }
  }
  for child in node.children.iter() {
    collect_background_fragments(child, abs, color, out);
  }
}

fn find_block_y_for_text(root: &FragmentNode, needle: &str) -> Option<f32> {
  fn walk(
    node: &FragmentNode,
    offset: Point,
    current_block_y: Option<f32>,
    needle: &str,
  ) -> Option<f32> {
    let abs = Point::new(offset.x + node.bounds.x(), offset.y + node.bounds.y());
    let current_block_y = if matches!(node.content, FragmentContent::Block { .. }) {
      Some(abs.y)
    } else {
      current_block_y
    };

    if let FragmentContent::Text { text, .. } = &node.content {
      if text.as_ref() == needle {
        return current_block_y;
      }
    }
    for child in node.children.iter() {
      if let Some(found) = walk(child, abs, current_block_y, needle) {
        return Some(found);
      }
    }
    None
  }

  walk(root, Point::ZERO, None, needle)
}

#[test]
fn display_contents_block_margin_collapse_matches_unwrapped() {
  let mut renderer = FastRender::builder()
    .font_sources(FontConfig::bundled_only())
    .build()
    .expect("build renderer");

  // The wrapper is styled to establish a BFC if it were to generate a box (`overflow: hidden`).
  // `display: contents` must suppress box generation entirely, so the layout must match the
  // unwrapped DOM.
  let with_wrapper = r#"<!doctype html>
    <style>
      html, body { margin: 0; padding: 0; }
      #outer { display: flow-root; }
      #prev { height: 10px; margin: 0 0 20px 0; background: #ccc; }
      #a { height: 10px; margin: 30px 0 5px 0; background: #0c0; }
      #b { height: 10px; margin: 15px 0 40px 0; background: #0cc; }
      #next { height: 10px; margin: 10px 0 0 0; background: #ccc; }
      #wrapper { display: contents; overflow: hidden; }
    </style>
    <div id="outer">
      <div id="prev">P</div>
      <div id="wrapper"><div id="a">A</div><div id="b">B</div></div>
      <div id="next">N</div>
    </div>
  "#;

  let unwrapped = r#"<!doctype html>
    <style>
      html, body { margin: 0; padding: 0; }
      #outer { display: flow-root; }
      #prev { height: 10px; margin: 0 0 20px 0; background: #ccc; }
      #a { height: 10px; margin: 30px 0 5px 0; background: #0c0; }
      #b { height: 10px; margin: 15px 0 40px 0; background: #0cc; }
      #next { height: 10px; margin: 10px 0 0 0; background: #ccc; }
    </style>
    <div id="outer">
      <div id="prev">P</div>
      <div id="a">A</div><div id="b">B</div>
      <div id="next">N</div>
    </div>
  "#;

  let wrapped_tree = layout_html(&mut renderer, with_wrapper);
  let unwrapped_tree = layout_html(&mut renderer, unwrapped);

  let wrapped_a_y = find_block_y_for_text(&wrapped_tree.root, "A").expect("find A block");
  let wrapped_b_y = find_block_y_for_text(&wrapped_tree.root, "B").expect("find B block");
  let unwrapped_a_y = find_block_y_for_text(&unwrapped_tree.root, "A").expect("find A block");
  let unwrapped_b_y = find_block_y_for_text(&unwrapped_tree.root, "B").expect("find B block");

  assert!(
    (wrapped_a_y - unwrapped_a_y).abs() <= EPS,
    "A y mismatch (wrapped={wrapped_a_y}, unwrapped={unwrapped_a_y})"
  );
  assert!(
    (wrapped_b_y - unwrapped_b_y).abs() <= EPS,
    "B y mismatch (wrapped={wrapped_b_y}, unwrapped={unwrapped_b_y})"
  );
}

#[test]
fn display_contents_inline_padding_is_ignored() {
  let mut renderer = FastRender::builder()
    .font_sources(FontConfig::bundled_only())
    .build()
    .expect("build renderer");

  let html = r#"<!doctype html>
    <style>
      html, body { margin: 0; padding: 0; }
      body { font-family: sans-serif; font-size: 20px; }
      #wrapper { display: contents; padding-left: 40px; border-left: 10px solid red; }
      #inner { color: blue; }
    </style>
    <div>a<span id="wrapper">b<span id="inner">c</span>d</span>e</div>
  "#;

  let tree = layout_html(&mut renderer, html);

  let mut frags = Vec::new();
  collect_text_fragments(&tree.root, Point::ZERO, &mut frags);

  let text: String = frags.iter().map(|(t, _, _)| t.as_str()).collect();
  assert!(
    text.contains("abcde"),
    "expected text to include abcde, got {text:?}"
  );

  // Find the x positions of each fragment in the sequence a b c d e.
  let mut seq = Vec::new();
  for (t, x, w) in &frags {
    if matches!(t.as_str(), "a" | "b" | "c" | "d" | "e") {
      seq.push((t.as_str().to_string(), *x, *w));
    }
  }
  // Ensure we found the intended fragments (some engines split text; only assert the boundary gaps).
  let expected = ["a", "b", "c", "d", "e"];
  assert_eq!(
    seq.iter().map(|(t, _, _)| t.as_str()).collect::<Vec<_>>(),
    expected
  );

  for window in seq.windows(2) {
    let (a_text, a_x, a_w) = &window[0];
    let (b_text, b_x, _) = &window[1];
    let end = a_x + a_w;
    assert!(
      (end - b_x).abs() <= 0.5,
      "expected inline text to be contiguous between {a_text:?} and {b_text:?} (end={end}, next_start={b_x})"
    );
  }
}

#[test]
fn display_contents_preserves_before_after_order() {
  let mut renderer = FastRender::builder()
    .font_sources(FontConfig::bundled_only())
    .build()
    .expect("build renderer");

  let html = r#"<!doctype html>
    <style>
      html, body { margin: 0; padding: 0; }
      #w { display: contents; }
      #w::before { content: "B"; }
      #w::after { content: "A"; }
    </style>
    <div><span id="w">X</span></div>
  "#;

  let tree = layout_html(&mut renderer, html);
  let mut text = String::new();
  collect_text(&tree.root, &mut text);
  assert!(
    text.contains("BXA"),
    "expected ::before/text/::after order to be preserved, got {text:?}"
  );
}

#[test]
fn display_contents_before_pseudo_can_participate_in_grid_layout() {
  let mut renderer = FastRender::builder()
    .font_sources(FontConfig::bundled_only())
    .build()
    .expect("build renderer");

  // Kotlinlang.org uses `display: contents` anchors whose `::before` pseudo-elements act as grid
  // spanning card backgrounds. The pseudo-element is empty aside from its background, so it must
  // still stretch to the grid track width.
  let html = r#"<!doctype html>
    <style>
      html, body { margin: 0; padding: 0; }
      #grid {
        display: grid;
        width: 300px;
        grid-template-columns: repeat(3, 1fr);
        grid-template-rows: 50px 50px;
        column-gap: 10px;
      }
      #w { display: contents; --col: 2; --row: 1; }
      #w::before {
        content: "";
        display: block;
        background: rgb(255, 0, 0);
        grid-column: var(--col);
        grid-row: var(--row) / calc(var(--row) + 2);
      }
      /* Ensure the contents also participate in the grid so the box tree is non-trivial. */
      #inner { grid-column: 2; grid-row: 1; }
    </style>
    <div id="grid">
      <a id="w"><div id="inner">X</div></a>
    </div>
  "#;

  let tree = layout_html(&mut renderer, html);
  let mut red_fragments = Vec::new();
  collect_background_fragments(&tree.root, Point::ZERO, Rgba::RED, &mut red_fragments);

  assert_eq!(
    red_fragments.len(),
    1,
    "expected exactly one red background fragment, got {red_fragments:?}"
  );

  let (x, _y, width, height) = red_fragments[0];
  assert!(
    height > 0.5,
    "expected ::before fragment to have non-zero height, got {height}"
  );
  assert!(
    width > 0.5,
    "expected ::before fragment to have non-zero width, got {width}"
  );

  let expected_width = (300.0 - 10.0 * 2.0) / 3.0;
  assert!(
    (width - expected_width).abs() < 1.0,
    "expected ::before width to match track width (expected={expected_width:.2}, got={width:.2})"
  );
  let expected_x = expected_width + 10.0;
  assert!(
    (x - expected_x).abs() < 2.0,
    "expected ::before x to align with column 2 (expected={expected_x:.2}, got={x:.2})"
  );
}
