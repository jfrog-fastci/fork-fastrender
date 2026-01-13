use crate::api::{FastRender, LayoutDocumentOptions, PageStacking, RenderOptions};
use crate::geometry::Point;
use crate::image_loader::ImageCache;
use crate::paint::display_list_renderer::PaintParallelism;
use crate::paint::painter::paint_tree_display_list_with_resources_scaled_offset;
use crate::scroll::ScrollState;
use crate::style::media::MediaType;
use crate::style::types::{BreakBetween, BreakInside};
use crate::tree::box_tree::ReplacedType;
use crate::tree::fragment_tree::{FragmentContent, FragmentNode, FragmentTree};
use crate::{LengthUnit, Rgba};
use regex::Regex;

fn pages<'a>(tree: &'a FragmentTree) -> Vec<&'a FragmentNode> {
  let mut roots = vec![&tree.root];
  roots.extend(tree.additional_fragments.iter());
  roots
}

fn page_document_wrapper<'a>(page: &'a FragmentNode) -> &'a FragmentNode {
  page.children.first().expect("page document wrapper")
}

fn page_content<'a>(page: &'a FragmentNode) -> &'a FragmentNode {
  page_document_wrapper(page)
    .children
    .first()
    .expect("page content")
}

fn page_content_start_y(page: &FragmentNode) -> f32 {
  page.bounds.y() + page_document_wrapper(page).bounds.y() + page_content(page).bounds.y()
}

fn find_text<'a>(node: &'a FragmentNode, needle: &str) -> Option<&'a FragmentNode> {
  if let FragmentContent::Text { text, .. } = &node.content {
    if text.contains(needle) {
      return Some(node);
    }
  }
  for child in node.children.iter() {
    if let Some(found) = find_text(child, needle) {
      return Some(found);
    }
  }
  None
}

fn find_in_margin_boxes<'a>(page: &'a FragmentNode, needle: &str) -> Option<&'a FragmentNode> {
  page
    .children
    .iter()
    .skip(1)
    .find_map(|child| find_text(child, needle))
}

fn find_margin_box_fragment<'a>(page: &'a FragmentNode, needle: &str) -> Option<&'a FragmentNode> {
  let needle = strip_ws(needle);
  page
    .children
    .iter()
    .skip(1)
    .find(|child| collected_text_compacted(child).contains(&needle))
}

fn strip_ws(s: &str) -> String {
  s.chars().filter(|c| !c.is_whitespace()).collect()
}

fn margin_boxes_contain_text(page: &FragmentNode, needle: &str) -> bool {
  let needle = strip_ws(needle);
  page
    .children
    .iter()
    .skip(1)
    .map(collected_text_compacted)
    .any(|text| text.contains(&needle))
}

fn find_text_eq<'a>(node: &'a FragmentNode, needle: &str) -> Option<&'a FragmentNode> {
  if let FragmentContent::Text { text, .. } = &node.content {
    if text.as_ref() == needle {
      return Some(node);
    }
  }
  for child in node.children.iter() {
    if let Some(found) = find_text_eq(child, needle) {
      return Some(found);
    }
  }
  None
}

#[derive(Debug, Clone)]
struct PositionedText {
  text: String,
  x: f32,
  y: f32,
}

fn collect_text_fragments(node: &FragmentNode, origin: (f32, f32), out: &mut Vec<PositionedText>) {
  let abs_x = origin.0 + node.bounds.x();
  let abs_y = origin.1 + node.bounds.y();
  if let FragmentContent::Text { text, .. } = &node.content {
    out.push(PositionedText {
      text: text.to_string(),
      x: abs_x,
      y: abs_y,
    });
  }
  for child in node.children.iter() {
    collect_text_fragments(child, (abs_x, abs_y), out);
  }
}

fn collected_text_compacted(node: &FragmentNode) -> String {
  let mut texts = Vec::new();
  collect_text_fragments(node, (0.0, 0.0), &mut texts);
  texts.sort_by(|a, b| {
    a.y
      .partial_cmp(&b.y)
      .unwrap_or(std::cmp::Ordering::Equal)
      .then(a.x.partial_cmp(&b.x).unwrap_or(std::cmp::Ordering::Equal))
  });
  let mut out = String::new();
  for t in texts {
    out.push_str(&t.text);
  }
  out.retain(|c| !c.is_whitespace());
  out
}

fn collected_page_content_texts_compacted(page_roots: &[&FragmentNode]) -> Vec<String> {
  page_roots
    .iter()
    .map(|page| collected_text_compacted(page_content(page)))
    .collect()
}

fn collect_label_sequence(page_roots: &[&FragmentNode], re: &Regex) -> Vec<String> {
  let mut labels = Vec::new();
  for page in page_roots {
    let text = collected_text_compacted(page_content(page));
    labels.extend(
      re.captures_iter(&text)
        .map(|cap| cap.get(1).expect("label group").as_str().to_string()),
    );
  }
  labels
}

fn token_words(prefix: &str, count: usize) -> Vec<String> {
  (0..count).map(|idx| format!("{prefix}{idx:03}")).collect()
}

fn collect_words_in_content(node: &FragmentNode) -> Vec<String> {
  let mut texts = Vec::new();
  collect_text_fragments(node, (0.0, 0.0), &mut texts);
  texts.sort_by(|a, b| {
    a.y
      .partial_cmp(&b.y)
      .unwrap_or(std::cmp::Ordering::Equal)
      .then(a.x.partial_cmp(&b.x).unwrap_or(std::cmp::Ordering::Equal))
  });
  let mut buf = String::new();
  for frag in texts {
    buf.push_str(&frag.text);
    buf.push(' ');
  }
  buf
    .split_whitespace()
    .filter(|word| {
      word.len() == 4 && word.as_bytes()[0] == b'w' && word[1..].bytes().all(|b| b.is_ascii_digit())
    })
    .map(|s| s.to_string())
    .collect()
}

fn collect_words_across_pages(pages: &[&FragmentNode]) -> Vec<String> {
  pages
    .iter()
    .flat_map(|page| {
      let content = page.children.first().expect("page content");
      collect_words_in_content(content)
    })
    .collect()
}
fn count_text_fragments_by_column(page: &FragmentNode, needle: &str) -> (usize, usize) {
  let content = page_content(page);
  let mut texts = Vec::new();
  collect_text_fragments(content, (0.0, 0.0), &mut texts);
  let split_x = content.bounds.x() + content.bounds.width() / 2.0;
  let mut left = 0usize;
  let mut right = 0usize;
  for t in texts {
    if t.text.trim() != needle {
      continue;
    }
    if t.x < split_x {
      left += 1;
    } else {
      right += 1;
    }
  }
  (left, right)
}

fn collect_floats<'a>(node: &'a FragmentNode, out: &mut Vec<&'a FragmentNode>) {
  if node
    .style
    .as_ref()
    .is_some_and(|style| style.float.is_floating())
  {
    out.push(node);
  }
  for child in node.children.iter() {
    collect_floats(child, out);
  }
}

fn find_text_with_parent<'a>(
  node: &'a FragmentNode,
  needle: &str,
) -> Option<(&'a FragmentNode, &'a FragmentNode)> {
  for child in node.children.iter() {
    if let FragmentContent::Text { text, .. } = &child.content {
      if text.contains(needle) {
        return Some((node, child));
      }
    }
    if let Some(found) = find_text_with_parent(child, needle) {
      return Some(found);
    }
  }
  None
}

fn find_replaced_image<'a>(node: &'a FragmentNode) -> Option<&'a FragmentNode> {
  if let FragmentContent::Replaced { replaced_type, .. } = &node.content {
    if matches!(replaced_type, ReplacedType::Image { .. }) {
      return Some(node);
    }
  }

  for child in node.children.iter() {
    if let Some(found) = find_replaced_image(child) {
      return Some(found);
    }
  }

  None
}

fn find_fragment_by_background<'a>(
  node: &'a FragmentNode,
  color: Rgba,
) -> Option<&'a FragmentNode> {
  if node.content.is_block()
    && node
      .style
      .as_ref()
      .is_some_and(|style| style.background_color == color)
  {
    return Some(node);
  }
  for child in node.children.iter() {
    if let Some(found) = find_fragment_by_background(child, color) {
      return Some(found);
    }
  }
  None
}

fn find_fragment_with_background<'a>(
  node: &'a FragmentNode,
  color: Rgba,
) -> Option<&'a FragmentNode> {
  if node
    .style
    .as_ref()
    .is_some_and(|style| style.background_color == color)
  {
    return Some(node);
  }
  for child in node.children.iter() {
    if let Some(found) = find_fragment_with_background(child, color) {
      return Some(found);
    }
  }
  None
}

fn assert_bounds_close(bounds: &crate::geometry::Rect, expected: (f32, f32, f32, f32)) {
  let (x, y, width, height) = expected;
  let epsilon = 0.01;
  assert!(
    (bounds.x() - x).abs() < epsilon,
    "x mismatch: actual {}, expected {}",
    bounds.x(),
    x
  );
  assert!(
    (bounds.y() - y).abs() < epsilon,
    "y mismatch: actual {}, expected {}",
    bounds.y(),
    y
  );
  assert!(
    (bounds.width() - width).abs() < epsilon,
    "width mismatch: actual {}, expected {}",
    bounds.width(),
    width
  );
  assert!(
    (bounds.height() - height).abs() < epsilon,
    "height mismatch: actual {}, expected {}",
    bounds.height(),
    height
  );
}

fn pixel(pixmap: &resvg::tiny_skia::Pixmap, x: u32, y: u32) -> [u8; 4] {
  let idx = (y as usize * pixmap.width() as usize + x as usize) * 4;
  let data = pixmap.data();
  [data[idx], data[idx + 1], data[idx + 2], data[idx + 3]]
}

#[test]
fn page_rule_sets_size_and_margins() {
  let html = r#"
    <html>
      <head>
        <style>
          @page { size: 200px 400px; margin: 20px 30px 40px 50px; }
        </style>
      </head>
      <body>
        <div style="height: 700px"></div>
      </body>
    </html>
  "#;

  let mut renderer = FastRender::new().unwrap();
  let dom = renderer.parse_html(html).unwrap();
  let tree = renderer
    .layout_document_for_media(&dom, 800, 1000, MediaType::Print)
    .unwrap();
  let page_roots = pages(&tree);

  assert!(page_roots.len() >= 2);
  assert!((page_roots[0].bounds.width() - 200.0).abs() < 0.1);
  assert!((page_roots[0].bounds.height() - 400.0).abs() < 0.1);

  let wrapper = page_document_wrapper(page_roots[0]);
  assert!((wrapper.bounds.x() - 50.0).abs() < 0.1);
  assert!((wrapper.bounds.y() - 20.0).abs() < 0.1);
  assert!((wrapper.bounds.height() - 340.0).abs() < 0.1);
  let content = page_content(page_roots[0]);
  assert!(
    (content.bounds.height() - 340.0).abs() < 0.1,
    "content bounds should be local to the wrapper"
  );
}

#[test]
fn margin_box_fixed_widths_are_positioned_per_css_page_three_box_algorithm() {
  let html = r#"
    <html>
      <head>
        <style>
          @page {
            size: 200px 200px;
            margin: 40px;
            @top-left { width: 30px; content: "A"; }
            @top-center { width: 40px; content: "B"; }
            @top-right { width: 30px; content: "C"; }
          }
          html, body { margin: 0; padding: 0; }
        </style>
      </head>
      <body>
        <div style="height: 1px"></div>
      </body>
    </html>
  "#;

  let mut renderer = FastRender::new().unwrap();
  let dom = renderer.parse_html(html).unwrap();
  let tree = renderer
    .layout_document_for_media(&dom, 400, 400, MediaType::Print)
    .unwrap();
  let page = pages(&tree)[0];

  let a = find_margin_box_fragment(page, "A").expect("expected @top-left margin box");
  let b = find_margin_box_fragment(page, "B").expect("expected @top-center margin box");
  let c = find_margin_box_fragment(page, "C").expect("expected @top-right margin box");

  let epsilon = 0.1;
  assert!(
    (a.bounds.x() - 40.0).abs() < epsilon,
    "top-left x mismatch: actual {}, expected 40",
    a.bounds.x()
  );
  assert!(
    (b.bounds.x() - 80.0).abs() < epsilon,
    "top-center x mismatch: actual {}, expected 80",
    b.bounds.x()
  );
  assert!(
    (c.bounds.x() - 130.0).abs() < epsilon,
    "top-right x mismatch: actual {}, expected 130",
    c.bounds.x()
  );

  assert!(
    (a.bounds.width() - 30.0).abs() < epsilon,
    "top-left width mismatch: actual {}, expected 30",
    a.bounds.width()
  );
  assert!(
    (b.bounds.width() - 40.0).abs() < epsilon,
    "top-center width mismatch: actual {}, expected 40",
    b.bounds.width()
  );
  assert!(
    (c.bounds.width() - 30.0).abs() < epsilon,
    "top-right width mismatch: actual {}, expected 30",
    c.bounds.width()
  );
}

#[test]
fn margin_box_max_width_clamps_used_size() {
  let html = r#"
    <html>
      <head>
        <style>
          @page {
            size: 200px 200px;
            margin: 40px;
            @top-center { width: 200px; max-width: 40px; content: "B"; }
          }
          html, body { margin: 0; padding: 0; }
        </style>
      </head>
      <body>
        <div style="height: 1px"></div>
      </body>
    </html>
  "#;

  let mut renderer = FastRender::new().unwrap();
  let dom = renderer.parse_html(html).unwrap();
  let tree = renderer
    .layout_document_for_media(&dom, 400, 400, MediaType::Print)
    .unwrap();
  let page = pages(&tree)[0];

  let b = find_margin_box_fragment(page, "B").expect("expected @top-center margin box");

  let epsilon = 0.1;
  assert!(
    (b.bounds.width() - 40.0).abs() < epsilon,
    "expected max-width to clamp used size to 40px, got {}",
    b.bounds.width()
  );
}

#[test]
fn margin_box_min_width_clamps_used_size() {
  let html = r#"
    <html>
      <head>
        <style>
          @page {
            size: 200px 200px;
            margin: 40px;
            @top-left { width: 10px; min-width: 50px; content: "A"; }
          }
          html, body { margin: 0; padding: 0; }
        </style>
      </head>
      <body>
        <div style="height: 1px"></div>
      </body>
    </html>
  "#;

  let mut renderer = FastRender::new().unwrap();
  let dom = renderer.parse_html(html).unwrap();
  let tree = renderer
    .layout_document_for_media(&dom, 400, 400, MediaType::Print)
    .unwrap();
  let page = pages(&tree)[0];

  let a = find_margin_box_fragment(page, "A").expect("expected @top-left margin box");

  let epsilon = 0.1;
  assert!(
    (a.bounds.width() - 50.0).abs() < epsilon,
    "expected min-width to clamp used size to 50px, got {}",
    a.bounds.width()
  );
}

#[test]
fn margin_box_fixed_heights_are_positioned_per_css_page_three_box_algorithm() {
  let html = r#"
    <html>
      <head>
        <style>
          @page {
            size: 200px 200px;
            margin: 40px;
            @left-top { height: 30px; content: "A"; }
            @left-middle { height: 40px; content: "B"; }
            @left-bottom { height: 30px; content: "C"; }
          }
          html, body { margin: 0; padding: 0; }
        </style>
      </head>
      <body>
        <div style="height: 1px"></div>
      </body>
    </html>
  "#;

  let mut renderer = FastRender::new().unwrap();
  let dom = renderer.parse_html(html).unwrap();
  let tree = renderer
    .layout_document_for_media(&dom, 400, 400, MediaType::Print)
    .unwrap();
  let page = pages(&tree)[0];

  let a = find_margin_box_fragment(page, "A").expect("expected @left-top margin box");
  let b = find_margin_box_fragment(page, "B").expect("expected @left-middle margin box");
  let c = find_margin_box_fragment(page, "C").expect("expected @left-bottom margin box");

  let epsilon = 0.1;
  assert!(
    (a.bounds.y() - 40.0).abs() < epsilon,
    "left-top y mismatch: actual {}, expected 40",
    a.bounds.y()
  );
  assert!(
    (b.bounds.y() - 80.0).abs() < epsilon,
    "left-middle y mismatch: actual {}, expected 80",
    b.bounds.y()
  );
  assert!(
    (c.bounds.y() - 130.0).abs() < epsilon,
    "left-bottom y mismatch: actual {}, expected 130",
    c.bounds.y()
  );

  assert!(
    (a.bounds.height() - 30.0).abs() < epsilon,
    "left-top height mismatch: actual {}, expected 30",
    a.bounds.height()
  );
  assert!(
    (b.bounds.height() - 40.0).abs() < epsilon,
    "left-middle height mismatch: actual {}, expected 40",
    b.bounds.height()
  );
  assert!(
    (c.bounds.height() - 30.0).abs() < epsilon,
    "left-bottom height mismatch: actual {}, expected 30",
    c.bounds.height()
  );
}

#[test]
fn margin_box_max_height_clamps_used_size() {
  let html = r#"
    <html>
      <head>
        <style>
          @page {
            size: 200px 200px;
            margin: 40px;
            @left-middle { height: 200px; max-height: 40px; content: "B"; }
          }
          html, body { margin: 0; padding: 0; }
        </style>
      </head>
      <body>
        <div style="height: 1px"></div>
      </body>
    </html>
  "#;

  let mut renderer = FastRender::new().unwrap();
  let dom = renderer.parse_html(html).unwrap();
  let tree = renderer
    .layout_document_for_media(&dom, 400, 400, MediaType::Print)
    .unwrap();
  let page = pages(&tree)[0];

  let b = find_margin_box_fragment(page, "B").expect("expected @left-middle margin box");

  let epsilon = 0.1;
  assert!(
    (b.bounds.height() - 40.0).abs() < epsilon,
    "expected max-height to clamp used size to 40px, got {}",
    b.bounds.height()
  );
}

#[test]
fn margin_box_min_height_clamps_used_size() {
  let html = r#"
    <html>
      <head>
        <style>
          @page {
            size: 200px 200px;
            margin: 40px;
            @left-top { height: 10px; min-height: 50px; content: "A"; }
          }
          html, body { margin: 0; padding: 0; }
        </style>
      </head>
      <body>
        <div style="height: 1px"></div>
      </body>
    </html>
  "#;

  let mut renderer = FastRender::new().unwrap();
  let dom = renderer.parse_html(html).unwrap();
  let tree = renderer
    .layout_document_for_media(&dom, 400, 400, MediaType::Print)
    .unwrap();
  let page = pages(&tree)[0];

  let a = find_margin_box_fragment(page, "A").expect("expected @left-top margin box");

  let epsilon = 0.1;
  assert!(
    (a.bounds.height() - 50.0).abs() < epsilon,
    "expected min-height to clamp used size to 50px, got {}",
    a.bounds.height()
  );
}

#[test]
fn margin_box_horizontal_margins_offset_border_box() {
  let html = r#"
    <html>
      <head>
        <style>
          @page {
            size: 200px 200px;
            margin: 40px;
            @top-left { width: 30px; margin-left: 5px; margin-right: 10px; content: "A"; }
            @top-center { width: 40px; content: "B"; }
            @top-right { width: 30px; content: "C"; }
          }
          html, body { margin: 0; padding: 0; }
        </style>
      </head>
      <body><div style="height: 1px"></div></body>
    </html>
  "#;

  let mut renderer = FastRender::new().unwrap();
  let dom = renderer.parse_html(html).unwrap();
  let tree = renderer
    .layout_document_for_media(&dom, 400, 400, MediaType::Print)
    .unwrap();
  let page = pages(&tree)[0];

  let a = find_margin_box_fragment(page, "A").expect("expected @top-left margin box");
  let b = find_margin_box_fragment(page, "B").expect("expected @top-center margin box");
  let c = find_margin_box_fragment(page, "C").expect("expected @top-right margin box");

  let epsilon = 0.1;
  assert!(
    (a.bounds.x() - 45.0).abs() < epsilon,
    "expected margins to offset border box start (x=45), got {}",
    a.bounds.x()
  );
  assert!(
    (a.bounds.width() - 30.0).abs() < epsilon,
    "expected border box width to exclude margins (width=30), got {}",
    a.bounds.width()
  );

  assert!(
    (b.bounds.x() - 80.0).abs() < epsilon,
    "expected top-center to remain centered (x=80), got {}",
    b.bounds.x()
  );
  assert!(
    (c.bounds.x() - 130.0).abs() < epsilon,
    "expected top-right to remain right-aligned (x=130), got {}",
    c.bounds.x()
  );
}

#[test]
fn margin_box_vertical_margins_offset_border_box() {
  let html = r#"
    <html>
      <head>
        <style>
          @page {
            size: 200px 200px;
            margin: 40px;
            @left-top { height: 30px; margin-top: 5px; margin-bottom: 10px; content: "A"; }
            @left-middle { height: 40px; content: "B"; }
            @left-bottom { height: 30px; content: "C"; }
          }
          html, body { margin: 0; padding: 0; }
        </style>
      </head>
      <body><div style="height: 1px"></div></body>
    </html>
  "#;

  let mut renderer = FastRender::new().unwrap();
  let dom = renderer.parse_html(html).unwrap();
  let tree = renderer
    .layout_document_for_media(&dom, 400, 400, MediaType::Print)
    .unwrap();
  let page = pages(&tree)[0];

  let a = find_margin_box_fragment(page, "A").expect("expected @left-top margin box");
  let b = find_margin_box_fragment(page, "B").expect("expected @left-middle margin box");
  let c = find_margin_box_fragment(page, "C").expect("expected @left-bottom margin box");

  let epsilon = 0.1;
  assert!(
    (a.bounds.y() - 45.0).abs() < epsilon,
    "expected margins to offset border box start (y=45), got {}",
    a.bounds.y()
  );
  assert!(
    (a.bounds.height() - 30.0).abs() < epsilon,
    "expected border box height to exclude margins (height=30), got {}",
    a.bounds.height()
  );

  assert!(
    (b.bounds.y() - 80.0).abs() < epsilon,
    "expected left-middle to remain centered (y=80), got {}",
    b.bounds.y()
  );
  assert!(
    (c.bounds.y() - 130.0).abs() < epsilon,
    "expected left-bottom to remain bottom-aligned (y=130), got {}",
    c.bounds.y()
  );
}

#[test]
fn margin_box_content_box_width_includes_padding() {
  let html = r#"
    <html>
      <head>
        <style>
          @page {
            size: 200px 200px;
            margin: 40px;
            @top-left { width: 30px; padding-left: 10px; padding-right: 10px; content: "A"; }
            @top-center { width: 20px; content: "B"; }
            @top-right { width: 30px; content: "C"; }
          }
          html, body { margin: 0; padding: 0; }
        </style>
      </head>
      <body><div style="height: 1px"></div></body>
    </html>
  "#;

  let mut renderer = FastRender::new().unwrap();
  let dom = renderer.parse_html(html).unwrap();
  let tree = renderer
    .layout_document_for_media(&dom, 400, 400, MediaType::Print)
    .unwrap();
  let page = pages(&tree)[0];

  let a = find_margin_box_fragment(page, "A").expect("expected @top-left margin box");
  let b = find_margin_box_fragment(page, "B").expect("expected @top-center margin box");
  let c = find_margin_box_fragment(page, "C").expect("expected @top-right margin box");

  let epsilon = 0.1;
  assert!(
    (a.bounds.width() - 50.0).abs() < epsilon,
    "expected content-box width to include padding (width=50), got {}",
    a.bounds.width()
  );
  assert!(
    (b.bounds.x() - 90.0).abs() < epsilon,
    "expected top-center to be centered based on its border box (x=90), got {}",
    b.bounds.x()
  );
  assert!(
    (c.bounds.x() - 130.0).abs() < epsilon,
    "expected top-right x=130, got {}",
    c.bounds.x()
  );
}

#[test]
fn page_rule_important_overrides_non_important() {
  let html = r#"
    <html>
      <head>
        <style>
          @page { size: 200px 200px; margin: 0; margin-top: 10px !important; }
          @page { margin-top: 30px; }
        </style>
      </head>
      <body>
        <div style="height: 50px"></div>
      </body>
    </html>
  "#;

  let mut renderer = FastRender::new().unwrap();
  let dom = renderer.parse_html(html).unwrap();
  let tree = renderer
    .layout_document_for_media(&dom, 400, 400, MediaType::Print)
    .unwrap();
  let page = pages(&tree)[0];

  let wrapper = page_document_wrapper(page);
  assert!(
    (wrapper.bounds.y() - 10.0).abs() < 0.1,
    "expected margin-top=10px from !important declaration; got {}",
    wrapper.bounds.y()
  );
}

#[test]
fn page_rule_layers_invert_for_important() {
  let html_normal = r#"
    <html>
      <head>
        <style>
          @layer a, b;
          @layer a { @page { size: 200px 200px; margin: 0; margin-top: 10px; } }
          @layer b { @page { margin-top: 20px; } }
        </style>
      </head>
      <body><div style="height: 50px"></div></body>
    </html>
  "#;

  let mut renderer = FastRender::new().unwrap();
  let dom = renderer.parse_html(html_normal).unwrap();
  let tree = renderer
    .layout_document_for_media(&dom, 400, 400, MediaType::Print)
    .unwrap();
  let page = pages(&tree)[0];
  let wrapper = page_document_wrapper(page);
  assert!(
    (wrapper.bounds.y() - 20.0).abs() < 0.1,
    "expected later layer b to win for normal declarations; got {}",
    wrapper.bounds.y()
  );

  let html_important = r#"
    <html>
      <head>
        <style>
          @layer a, b;
          @layer a { @page { size: 200px 200px; margin: 0; margin-top: 10px !important; } }
          @layer b { @page { margin-top: 20px !important; } }
        </style>
      </head>
      <body><div style="height: 50px"></div></body>
    </html>
  "#;

  let dom = renderer.parse_html(html_important).unwrap();
  let tree = renderer
    .layout_document_for_media(&dom, 400, 400, MediaType::Print)
    .unwrap();
  let page = pages(&tree)[0];
  let wrapper = page_document_wrapper(page);
  assert!(
    (wrapper.bounds.y() - 10.0).abs() < 0.1,
    "expected earlier layer a to win for !important declarations; got {}",
    wrapper.bounds.y()
  );
}

#[test]
fn page_rule_left_and_right_offsets_differ() {
  let html = r#"
    <html>
      <head>
        <style>
          @page { size: 200px 300px; margin-top: 0; margin-bottom: 0; }
          @page :right { margin-left: 10px; margin-right: 30px; }
          @page :left { margin-left: 40px; margin-right: 5px; }
        </style>
      </head>
      <body>
        <div style="height: 600px"></div>
      </body>
    </html>
  "#;

  let mut renderer = FastRender::new().unwrap();
  let dom = renderer.parse_html(html).unwrap();
  let tree = renderer
    .layout_document_for_media(&dom, 400, 400, MediaType::Print)
    .unwrap();
  let page_roots = pages(&tree);

  assert!(page_roots.len() >= 2);
  let first = page_document_wrapper(page_roots[0]);
  let second = page_document_wrapper(page_roots[1]);

  assert!((first.bounds.x() - 10.0).abs() < 0.1);
  assert!((second.bounds.x() - 40.0).abs() < 0.1);
  assert!((first.bounds.width() - 160.0).abs() < 0.1);
  assert!((second.bounds.width() - 155.0).abs() < 0.1);
}

fn collect_line_widths(node: &FragmentNode, out: &mut Vec<f32>) {
  if let FragmentContent::Line { .. } = node.content {
    out.push(node.bounds.width());
  }
  for child in node.children.iter() {
    collect_line_widths(child, out);
  }
}

#[test]
fn line_wrapping_respects_page_side_widths() {
  let html = r#"
    <html>
      <head>
        <style>
          @page { size: 200px 140px; margin: 10px; }
          @page :right { margin-left: 10px; margin-right: 10px; }
          @page :left { margin-left: 40px; margin-right: 10px; }
          body { margin: 0; }
          p { margin: 0; font-size: 16px; line-height: 16px; }
        </style>
      </head>
      <body>
        <p>
          This is a very long line of text that should wrap across multiple lines and pages so that
          we can verify pagination reflows content differently on right and left pages when margins
          change between them. The content intentionally repeats to ensure it spans at least two
          pages worth of text.
          This is a very long line of text that should wrap across multiple lines and pages so that
          we can verify pagination reflows content differently on right and left pages when margins
          change between them. The content intentionally repeats to ensure it spans at least two
          pages worth of text.
          This is a very long line of text that should wrap across multiple lines and pages so that
          we can verify pagination reflows content differently on right and left pages when margins
          change between them. The content intentionally repeats to ensure it spans at least two
          pages worth of text.
        </p>
      </body>
    </html>
  "#;

  let mut renderer = FastRender::new().unwrap();
  let dom = renderer.parse_html(html).unwrap();
  let tree = renderer
    .layout_document_for_media(&dom, 400, 400, MediaType::Print)
    .unwrap();
  let page_roots = pages(&tree);

  assert!(
    page_roots.len() >= 2,
    "expected at least two pages for wrapping test"
  );

  let mut first_lines = Vec::new();
  collect_line_widths(page_roots[0], &mut first_lines);
  let mut second_lines = Vec::new();
  collect_line_widths(page_roots[1], &mut second_lines);

  assert!(!first_lines.is_empty());
  assert!(!second_lines.is_empty());

  let first_max = first_lines.iter().cloned().fold(0.0, f32::max);
  let second_max = second_lines.iter().cloned().fold(0.0, f32::max);

  assert!(
    first_max > second_max + 5.0,
    "expected wider right page lines ({first_max}) than left page ({second_max})"
  );
}

#[test]
fn left_right_page_relayout_does_not_skip_or_duplicate_text() {
  let mut tokens = String::new();
  for idx in 0..100 {
    tokens.push_str(&format!(r#"<span class="label">[L{idx:03}]</span> "#));
    // Keep the filler consistent so every page reflow produces a stable ordering, while still
    // causing substantial wrapping changes between left/right pages.
    tokens.push_str(
      "lorem ipsum dolor sit amet consectetur adipiscing elit sed do eiusmod tempor incididunt ut labore et dolore magna aliqua ",
    );
  }

  let html = format!(
    r#"
    <html>
      <head>
        <style>
          @page {{ size: 240px 120px; margin-top: 10px; margin-bottom: 10px; }}
          @page :right {{ margin-left: 10px; margin-right: 10px; }}
          @page :left {{ margin-left: 90px; margin-right: 10px; }}
          body {{ margin: 0; font-size: 14px; line-height: 16px; }}
          p {{ margin: 0; }}
          .label {{ white-space: nowrap; }}
        </style>
      </head>
      <body><p>{tokens}</p></body>
    </html>
  "#,
    tokens = tokens
  );

  let mut renderer = FastRender::new().unwrap();
  let dom = renderer.parse_html(&html).unwrap();
  let tree = renderer
    .layout_document_for_media(&dom, 800, 600, MediaType::Print)
    .unwrap();
  let page_roots = pages(&tree);

  assert!(
    page_roots.len() >= 4,
    "expected multiple pages with alternating sides; got {} pages",
    page_roots.len()
  );

  let width_right = page_content(page_roots[0]).bounds.width();
  let width_left = page_content(page_roots[1]).bounds.width();
  assert!(
    width_right > width_left + 40.0,
    "expected :right page content to be significantly wider than :left (right={width_right}, left={width_left})"
  );

  let re = Regex::new(r"\[(L\d{3})\]").unwrap();
  let labels = collect_label_sequence(&page_roots, &re);
  let expected: Vec<String> = (0..100).map(|idx| format!("L{idx:03}")).collect();
  assert_eq!(
    labels,
    expected,
    "pagination must not skip/duplicate or reorder labels; page_texts={:?}",
    collected_page_content_texts_compacted(&page_roots)
  );
}

#[test]
fn pagination_does_not_skip_or_duplicate_when_left_right_widths_alternate() {
  let words = token_words("w", 300);
  let html = format!(
    r#"
    <html>
      <head>
        <style>
          @page {{ size: 200px 140px; margin-top: 10px; margin-bottom: 10px; }}
          @page :left {{ margin-left: 10px; margin-right: 50px; }}
          @page :right {{ margin-left: 50px; margin-right: 10px; }}
          body {{ margin: 0; }}
          p {{ margin: 0; font-size: 16px; line-height: 16px; }}
        </style>
      </head>
      <body>
        <p>{}</p>
      </body>
    </html>
  "#,
    words.join(" ")
  );

  let mut renderer = FastRender::new().unwrap();
  let dom = renderer.parse_html(&html).unwrap();
  let tree = renderer
    .layout_document_for_media(&dom, 400, 400, MediaType::Print)
    .unwrap();
  let page_roots = pages(&tree);
  assert!(
    page_roots.len() >= 2,
    "expected content to paginate across multiple pages"
  );

  let actual = collect_words_across_pages(&page_roots);
  assert_eq!(
    actual,
    words,
    "expected token stream to be preserved across pages; got_len={} pages={}",
    actual.len(),
    page_roots.len()
  );
}

#[test]
fn pagination_does_not_skip_or_duplicate_across_named_page_size_transition() {
  let words = token_words("w", 300);
  let split = 150usize;
  let html = format!(
    r#"
    <html>
      <head>
        <style>
          @page a {{ size: 200px 200px; margin: 0; }}
          @page b {{ size: 240px 200px; margin: 0; }}
          body {{ margin: 0; }}
          p {{ margin: 0; font-size: 16px; line-height: 16px; }}
          .a {{ page: a; }}
          .b {{ page: b; }}
        </style>
      </head>
      <body>
        <div class="a"><p>{}</p></div>
        <div class="b"><p>{}</p></div>
      </body>
    </html>
  "#,
    words[..split].join(" "),
    words[split..].join(" ")
  );

  let mut renderer = FastRender::new().unwrap();
  let dom = renderer.parse_html(&html).unwrap();
  let tree = renderer
    .layout_document_for_media(&dom, 400, 400, MediaType::Print)
    .unwrap();
  let page_roots = pages(&tree);
  assert!(
    page_roots.len() >= 2,
    "expected content to paginate across multiple pages"
  );

  assert!(
    page_roots
      .iter()
      .any(|page| (page.bounds.width() - 240.0).abs() < 0.1),
    "expected at least one page with the 'b' size"
  );

  let actual = collect_words_across_pages(&page_roots);
  assert_eq!(
    actual,
    words,
    "expected token stream to be preserved across named page transition; got_len={} pages={}",
    actual.len(),
    page_roots.len()
  );
}

#[test]
fn named_pages_change_page_size() {
  let html = r#"
    <html>
      <head>
        <style>
          @page chapter { size: 300px 200px; margin: 10px; }
          @page { size: 200px 200px; margin: 10px; }
          div { page: chapter; }
        </style>
      </head>
      <body>
        <div style="height: 260px"></div>
      </body>
    </html>
  "#;

  let mut renderer = FastRender::new().unwrap();
  let dom = renderer.parse_html(html).unwrap();
  let tree = renderer
    .layout_document_for_media(&dom, 400, 400, MediaType::Print)
    .unwrap();
  let page_roots = pages(&tree);

  assert!((page_roots[0].bounds.width() - 300.0).abs() < 0.1);
  let content = page_content(page_roots[0]);
  assert!((content.bounds.width() - 280.0).abs() < 0.1);
}

#[test]
fn page_name_change_forces_page_boundary() {
  let html = r#"
    <html>
      <head>
        <style>
          @page { size: 200px 200px; margin: 0; }
          @page chapter { size: 300px 200px; margin: 0; }
          body { margin: 0; }
          #preface { height: 150px; }
          #chapter { page: chapter; height: 300px; }
        </style>
      </head>
      <body>
        <div id="preface">Preface</div>
        <div id="chapter">Chapter</div>
      </body>
    </html>
  "#;

  let mut renderer = FastRender::new().unwrap();
  let dom = renderer.parse_html(html).unwrap();
  let tree = renderer
    .layout_document_for_media(&dom, 400, 400, MediaType::Print)
    .unwrap();
  let page_roots = pages(&tree);

  assert!(page_roots.len() >= 2);
  assert!((page_roots[0].bounds.width() - 200.0).abs() < 0.1);
  assert!((page_roots[1].bounds.width() - 300.0).abs() < 0.1);

  let first_page = page_roots[0];
  assert!(find_text(first_page, "Preface").is_some());
  assert!(find_text(first_page, "Chapter").is_none());

  let second_page = page_roots[1];
  assert!(find_text(second_page, "Chapter").is_some());
}

#[test]
fn named_page_size_change_mid_document_does_not_skip_or_duplicate_text() {
  let mut preface = String::new();
  for idx in 0..50 {
    preface.push_str(&format!(r#"<span class="label">[P{idx:03}]</span> "#));
    preface
      .push_str("preface words that wrap differently depending on the used page size and margins ");
  }

  let mut chapter = String::new();
  for idx in 0..50 {
    chapter.push_str(&format!(r#"<span class="label">[C{idx:03}]</span> "#));
    chapter
      .push_str("chapter words that wrap differently depending on the used page size and margins ");
  }

  let html = format!(
    r#"
    <html>
      <head>
        <style>
          @page {{ size: 220px 130px; margin: 10px; }}
          @page chapter {{ size: 320px 180px; margin: 20px; }}
          body {{ margin: 0; font-size: 14px; line-height: 16px; }}
          p {{ margin: 0; }}
          #chapter {{ page: chapter; }}
          .label {{ white-space: nowrap; }}
        </style>
      </head>
      <body>
        <p>{preface}</p>
        <div id="chapter"><p>{chapter}</p></div>
      </body>
    </html>
  "#,
    preface = preface,
    chapter = chapter
  );

  let mut renderer = FastRender::new().unwrap();
  let dom = renderer.parse_html(&html).unwrap();
  let tree = renderer
    .layout_document_for_media(&dom, 800, 600, MediaType::Print)
    .unwrap();
  let page_roots = pages(&tree);
  assert!(
    page_roots.len() >= 4,
    "expected multiple pages before and after the page-name transition; got {} pages",
    page_roots.len()
  );

  let re = Regex::new(r"\[([PC]\d{3})\]").unwrap();
  let labels = collect_label_sequence(&page_roots, &re);
  let mut expected: Vec<String> = (0..50).map(|idx| format!("P{idx:03}")).collect();
  expected.extend((0..50).map(|idx| format!("C{idx:03}")));
  assert_eq!(
    labels,
    expected,
    "pagination must preserve content across page-size changes; page_texts={:?}",
    collected_page_content_texts_compacted(&page_roots)
  );

  for (idx, page) in page_roots.iter().enumerate() {
    let text = collected_text_compacted(page_content(page));
    let has_preface = text.contains("[P");
    let has_chapter = text.contains("[C");
    assert!(
      !(has_preface && has_chapter),
      "page {} should not mix preface + chapter content; text={}",
      idx + 1,
      text
    );
    if has_preface {
      assert!(
        (page.bounds.width() - 220.0).abs() < 0.1,
        "preface pages should use default @page size"
      );
    }
    if has_chapter {
      assert!(
        (page.bounds.width() - 320.0).abs() < 0.1,
        "chapter pages should use named @page size"
      );
    }
  }

  let preface_last_page = page_roots
    .iter()
    .position(|page| find_text(page_content(page), "P049").is_some())
    .expect("last preface label should exist");
  let chapter_first_page = page_roots
    .iter()
    .position(|page| find_text(page_content(page), "C000").is_some())
    .expect("first chapter label should exist");
  assert!(
    chapter_first_page > preface_last_page,
    "named page transition should force a clean page boundary (preface ends on page {}, chapter starts on page {})",
    preface_last_page + 1,
    chapter_first_page + 1
  );
}

#[test]
fn nested_page_override_and_revert_forces_boundaries() {
  let html = r#"
    <html>
      <head>
        <style>
          @page chapter { size: 260px 200px; margin: 0; }
          @page sub { size: 320px 200px; margin: 0; }
          body { margin: 0; page: chapter; }
          #sub { page: sub; }
          .block { height: 40px; }
        </style>
      </head>
      <body>
        <div class="block">Preface</div>
        <div id="sub" class="block">Sub</div>
        <div class="block">After</div>
      </body>
    </html>
  "#;

  let mut renderer = FastRender::new().unwrap();
  let dom = renderer.parse_html(html).unwrap();
  let tree = renderer
    .layout_document_for_media(&dom, 400, 400, MediaType::Print)
    .unwrap();
  let page_roots = pages(&tree);

  assert!(page_roots.len() >= 3);
  assert!((page_roots[0].bounds.width() - 260.0).abs() < 0.1);
  assert!((page_roots[1].bounds.width() - 320.0).abs() < 0.1);
  assert!((page_roots[2].bounds.width() - 260.0).abs() < 0.1);

  assert!(find_text(page_roots[0], "Preface").is_some());
  assert!(find_text(page_roots[0], "Sub").is_none());
  assert!(find_text(page_roots[0], "After").is_none());

  assert!(find_text(page_roots[1], "Sub").is_some());
  assert!(find_text(page_roots[1], "Preface").is_none());
  assert!(find_text(page_roots[1], "After").is_none());

  assert!(find_text(page_roots[2], "After").is_some());
  assert!(find_text(page_roots[2], "Preface").is_none());
  assert!(find_text(page_roots[2], "Sub").is_none());
}

#[test]
fn page_start_value_propagates_from_first_child() {
  let html = r#"
    <html>
      <head>
        <style>
          @page foo { size: 240px 200px; margin: 0; }
          @page bar { size: 320px 200px; margin: 0; }
          body { margin: 0; page: foo; }
          #bar { page: bar; height: 40px; }
          .block { height: 40px; }
        </style>
      </head>
      <body><div id="bar">Bar</div><div class="block">Foo</div></body>
    </html>
  "#;

  let mut renderer = FastRender::new().unwrap();
  let dom = renderer.parse_html(html).unwrap();
  let tree = renderer
    .layout_document_for_media(&dom, 400, 400, MediaType::Print)
    .unwrap();
  let page_roots = pages(&tree);

  assert!(page_roots.len() >= 2);
  assert!((page_roots[0].bounds.width() - 320.0).abs() < 0.1);
  assert!((page_roots[1].bounds.width() - 240.0).abs() < 0.1);

  assert!(find_text(page_roots[0], "Bar").is_some());
  assert!(find_text(page_roots[0], "Foo").is_none());

  assert!(find_text(page_roots[1], "Foo").is_some());
}

#[test]
fn named_page_boundaries_follow_fragmentation_axis_in_vertical_writing_mode() {
  let html = r#"
    <html>
      <head>
        <style>
          html { writing-mode: vertical-rl; }
          @page { size: 200px 200px; margin: 0; }
          @page chapter { size: 320px 200px; margin: 0; }
          body { margin: 0; }
          .block { width: 80px; height: 50px; }
          #chapter { page: chapter; }
        </style>
      </head>
      <body><div class="block">One</div><div id="chapter" class="block">Two</div></body>
    </html>
  "#;

  let mut renderer = FastRender::new().unwrap();
  let dom = renderer.parse_html(html).unwrap();
  let tree = renderer
    .layout_document_for_media(&dom, 400, 400, MediaType::Print)
    .unwrap();
  let page_roots = pages(&tree);

  assert!(page_roots.len() >= 2);
  assert!((page_roots[0].bounds.width() - 200.0).abs() < 0.1);
  assert!((page_roots[1].bounds.width() - 320.0).abs() < 0.1);

  assert!(
    find_text(page_roots[0], "One").is_some(),
    "page1 should contain \"One\"; got texts={:?}",
    page_roots
      .iter()
      .take(4)
      .map(|page| collected_text_compacted(page))
      .collect::<Vec<_>>()
  );
  assert!(find_text(page_roots[0], "Two").is_none());

  assert!(find_text(page_roots[1], "Two").is_some());
}

#[test]
fn multicol_pagination_uses_physical_height() {
  let html = r#"
    <html>
      <head>
        <style>
          @page { size: 200px 200px; margin: 0; }
          body { margin: 0; }
          .multi { column-count: 2; column-gap: 0; }
          .section { height: 150px; }
        </style>
      </head>
      <body>
        <div class="multi">
          <div class="section">First</div>
          <div class="section">Second</div>
          <div class="section">Third</div>
        </div>
      </body>
    </html>
  "#;

  let mut renderer = FastRender::new().unwrap();
  let dom = renderer.parse_html(html).unwrap();
  let tree = renderer
    .layout_document_for_media(&dom, 200, 200, MediaType::Print)
    .unwrap();
  let page_roots = pages(&tree);

  assert_eq!(
    page_roots.len(),
    2,
    "multi-column flow should not produce a trailing blank page"
  );

  let second_page = page_roots[1];
  assert!(
    find_text(second_page, "Third").is_some(),
    "content from the final column set should render on the second page"
  );
}

#[test]
fn multicol_fragmentainer_path_column_metadata_survives_pagination() {
  let html = r#"
    <html>
      <head>
        <style>
          @page { size: 200px 200px; margin: 0; }
          body { margin: 0; }
          .multi { column-count: 2; column-gap: 0; }
          .right { break-before: column; }
        </style>
      </head>
      <body>
        <div class="multi">
          <div>Left</div>
          <div class="right">Right</div>
        </div>
      </body>
    </html>
  "#;

  let mut renderer = FastRender::new().unwrap();
  let dom = renderer.parse_html(html).unwrap();
  let tree = renderer
    .layout_document_for_media(&dom, 200, 200, MediaType::Print)
    .unwrap();
  let page_roots = pages(&tree);
  assert_eq!(page_roots.len(), 1);

  let page = page_roots[0];
  let left = find_text(page_content(page), "Left").expect("left text fragment");
  let right = find_text(page_content(page), "Right").expect("right text fragment");

  assert_eq!(left.fragmentainer.page_index, 0);
  assert_eq!(right.fragmentainer.page_index, 0);

  assert_eq!(left.fragmentainer.column_set_index, Some(0));
  assert_eq!(left.fragmentainer.column_index, Some(0));
  assert_eq!(right.fragmentainer.column_set_index, Some(0));
  assert_eq!(right.fragmentainer.column_index, Some(1));
}

#[test]
fn trailing_pages_with_only_fixed_content_are_suppressed() {
  let html = r#"
    <html>
      <head>
        <style>
          @page { size: 200px 200px; margin: 0; }
          body { margin: 0 0 500px 0; }
          .header { position: fixed; top: 0; left: 0; height: 20px; }
          .content { height: 10px; }
        </style>
      </head>
      <body>
        <div class="header">FixedHeader</div>
        <div class="content">Content</div>
      </body>
    </html>
  "#;

  let mut renderer = FastRender::new().unwrap();
  let dom = renderer.parse_html(html).unwrap();
  let tree = renderer
    .layout_document_for_media(&dom, 200, 200, MediaType::Print)
    .unwrap();
  let page_roots = pages(&tree);

  assert_eq!(
    page_roots.len(),
    1,
    "trailing margin should not force an empty extra page"
  );
}

#[test]
fn margin_box_content_is_positioned_in_margins() {
  let html = r#"
    <html>
      <head>
        <style>
          @page {
            size: 200px 200px;
            margin: 20px;
            @top-center { content: "Header"; }
            @bottom-center { content: "Footer"; }
          }
        </style>
      </head>
      <body>
        <div style="height: 50px"></div>
      </body>
    </html>
  "#;

  let mut renderer = FastRender::new().unwrap();
  let dom = renderer.parse_html(html).unwrap();
  let tree = renderer
    .layout_document_for_media(&dom, 400, 400, MediaType::Print)
    .unwrap();
  let page_roots = pages(&tree);
  let page = page_roots[0];

  let content_y = page_content_start_y(page);
  let header_y = find_text_position(page, "Header", (0.0, 0.0))
    .expect("header margin box")
    .1;
  let footer_y = find_text_position(page, "Footer", (0.0, 0.0))
    .expect("footer margin box")
    .1;

  assert!(header_y < content_y);
  assert!(footer_y > content_y);
}

#[test]
fn running_header_carries_forward() {
  let html = r#"
     <html>
       <head>
         <style>
          @page {
            size: 200px 200px;
            margin: 20px;
            @top-center { content: element(header); }
          }
          h1 { position: running(header); }
        </style>
      </head>
      <body>
        <h1>Chapter Title</h1>
        <div style="height: 400px"></div>
      </body>
    </html>
  "#;

  let mut renderer = FastRender::new().unwrap();
  let dom = renderer.parse_html(html).unwrap();
  let tree = renderer
    .layout_document_for_media(&dom, 400, 400, MediaType::Print)
    .unwrap();
  let page_roots = pages(&tree);

  assert!(page_roots.len() >= 2);
  let page1 = page_roots[0];
  let page2 = page_roots[1];

  assert!(
    margin_boxes_contain_text(page1, "Chapter Title"),
    "page 1 should show running header in margin"
  );
  assert!(
    margin_boxes_contain_text(page2, "Chapter Title"),
    "page 2 should carry forward running header"
  );
}

#[test]
fn running_element_in_margin_box_is_inserted_in_content_order() {
  let html = r#"
     <html>
       <head>
         <style>
          @page {
            size: 200px 200px;
            margin: 40px 20px;
            @top-center { content: "A-" element(header) "-B"; }
          }
          body { margin: 0; }
          .running { position: running(header); margin: 0; font-size: 16px; line-height: 16px; }
        </style>
      </head>
      <body>
        <span class="running">Title</span>
        <div style="height: 400px"></div>
      </body>
    </html>
  "#;

  let mut renderer = FastRender::new().unwrap();
  let dom = renderer.parse_html(html).unwrap();
  let tree = renderer
    .layout_document_for_media(&dom, 400, 400, MediaType::Print)
    .unwrap();
  let page_roots = pages(&tree);

  assert!(page_roots.len() >= 2);
  assert!(
    margin_boxes_contain_text(page_roots[0], "A-Title-B"),
    "page 1 margin box should contain running element in content order"
  );
  assert!(
    margin_boxes_contain_text(page_roots[1], "A-Title-B"),
    "page 2 margin box should contain running element in content order"
  );
}

#[test]
fn running_element_margin_box_respects_padding() {
  let html = r#"
     <html>
       <head>
         <style>
          @page {
            size: 200px 200px;
            margin: 40px 20px;
            @top-center { content: element(header); padding-top: 10px; }
          }
          body { margin: 0; }
          .running { position: running(header); margin: 0; font-size: 16px; line-height: 16px; }
        </style>
      </head>
      <body>
        <span class="running">Title</span>
        <div style="height: 400px"></div>
      </body>
    </html>
  "#;

  let mut renderer = FastRender::new().unwrap();
  let dom = renderer.parse_html(html).unwrap();
  let tree = renderer
    .layout_document_for_media(&dom, 400, 400, MediaType::Print)
    .unwrap();
  let page = pages(&tree)[0];

  let margin_box = page
    .children
    .iter()
    .skip(1)
    .find(|child| find_text(child, "Title").is_some())
    .expect("expected running element in margin box");

  let text_pos =
    find_text_position(margin_box, "Title", (0.0, 0.0)).expect("expected Title text in margin box");
  let local_y = text_pos.1 - margin_box.bounds.y();
  assert!(
    local_y >= 9.0,
    "expected running element to be laid out inside padding box, got local_y={local_y}"
  );
}

#[test]
fn running_element_inside_style_containment_does_not_affect_margin_boxes() {
  let html = r#"
    <html>
      <head>
        <style>
          @page {
            size: 200px 200px;
            margin: 20px;
            @top-center { content: element(header); }
          }
          body { margin: 0; }
          h1 { margin: 0; font-size: 16px; line-height: 16px; }

          #outside { position: running(header); }
          #contained {
            contain: style;
            break-before: page;
          }
          #contained h1 { position: running(header); }
        </style>
      </head>
      <body>
        <h1 id="outside">Outside</h1>
        <div style="height: 40px"></div>
        <div id="contained">
          <h1>Inside</h1>
          <div style="height: 40px"></div>
        </div>
      </body>
    </html>
  "#;

  let mut renderer = FastRender::new().unwrap();
  let dom = renderer.parse_html(html).unwrap();
  let tree = renderer
    .layout_document_for_media(&dom, 400, 400, MediaType::Print)
    .unwrap();
  let page_roots = pages(&tree);

  assert!(page_roots.len() >= 2, "expected at least two pages");
  assert!(
    margin_boxes_contain_text(page_roots[0], "Outside"),
    "first page header should use the outside running element"
  );
  assert!(
    margin_boxes_contain_text(page_roots[1], "Outside"),
    "style-contained subtree must not update the running element outside"
  );
  assert!(
    !margin_boxes_contain_text(page_roots[1], "Inside"),
    "style-contained subtree must not expose its running element"
  );
}

#[test]
fn element_last_uses_last_anchor_on_page() {
  let html = r#"
    <html>
      <head>
        <style>
          @page {
            size: 200px 200px;
            margin: 20px;
            @top-center { content: element(header, last); }
          }
          h2 { position: running(header); }
        </style>
      </head>
      <body>
        <h2>First Header</h2>
        <h2>Second Header</h2>
        <div style="height: 50px"></div>
      </body>
    </html>
  "#;

  let mut renderer = FastRender::new().unwrap();
  let dom = renderer.parse_html(html).unwrap();
  let tree = renderer
    .layout_document_for_media(&dom, 400, 400, MediaType::Print)
    .unwrap();
  let page = pages(&tree)[0];

  let margin_texts: Vec<String> = page
    .children
    .iter()
    .skip(1)
    .map(collected_text_compacted)
    .collect();
  assert!(
    margin_texts
      .iter()
      .any(|text| text.contains("SecondHeader")),
    "element(header, last) should pick the last running element on the page"
  );
  assert!(
    !margin_texts.iter().any(|text| text.contains("FirstHeader")),
    "element(header, last) should not pick the first running element"
  );
}

#[test]
fn running_element_in_flex_is_out_of_flow_and_available_to_margin_boxes() {
  let html = r#"
    <html>
      <head>
        <style>
          @page {
            size: 200px 200px;
            margin: 20px;
            @top-center { content: element(header); }
          }
          body { margin: 0; }
          .flex { display: flex; flex-direction: column; gap: 8px; }
          .running { position: running(header); padding: 4px; }
          .tall { height: 400px; }
        </style>
      </head>
      <body>
        <div class="flex">
          <div class="running">Flex Header</div>
          <div class="tall">Body content</div>
        </div>
      </body>
    </html>
  "#;

  let mut renderer = FastRender::new().unwrap();
  let dom = renderer.parse_html(html).unwrap();
  let tree = renderer
    .layout_document_for_media(&dom, 200, 200, MediaType::Print)
    .unwrap();
  let page_roots = pages(&tree);

  assert!(
    page_roots.len() >= 2,
    "tall content should paginate across multiple pages"
  );

  let first_page = page_roots[0];
  let content = page_content(first_page);
  assert!(
    !collected_text_compacted(content).contains("FlexHeader"),
    "running element should not consume space in main content"
  );

  assert!(
    margin_boxes_contain_text(first_page, "Flex Header"),
    "running element should be available to margin boxes"
  );
  let header_y = find_text_position(first_page, "Flex", (0.0, 0.0))
    .expect("running header in margin box")
    .1;
  let content_y = page_content_start_y(first_page);
  assert!(
    header_y < content_y,
    "header should appear in the page margin area"
  );
}

#[test]
fn running_element_in_grid_is_available_to_margin_boxes() {
  let html = r#"
    <html>
      <head>
        <style>
          @page {
            size: 200px 200px;
            margin: 20px;
            @top-left { content: element(header); }
          }
          body { margin: 0; }
          .grid { display: grid; grid-template-columns: 40px 60px; }
          .running { position: running(header); grid-column: 2 / 3; padding: 4px; }
          .cell { padding: 4px; }
          .tall { height: 400px; }
        </style>
      </head>
      <body>
        <div class="grid">
          <div class="running">Grid Header</div>
          <div class="cell">Cell</div>
        </div>
        <div class="tall">Body content</div>
      </body>
    </html>
  "#;

  let mut renderer = FastRender::new().unwrap();
  let dom = renderer.parse_html(html).unwrap();
  let tree = renderer
    .layout_document_for_media(&dom, 200, 200, MediaType::Print)
    .unwrap();
  let page_roots = pages(&tree);

  assert!(
    page_roots.len() >= 2,
    "tall content should paginate across multiple pages"
  );

  let first_page = page_roots[0];
  let content = page_content(first_page);
  assert!(
    !collected_text_compacted(content).contains("GridHeader"),
    "running element should not paint in normal flow"
  );

  assert!(
    margin_boxes_contain_text(first_page, "Grid Header"),
    "running element should be available to margin boxes"
  );
  let header_y = find_text_position(first_page, "Grid", (0.0, 0.0))
    .expect("running header in margin box")
    .1;
  let content_y = page_content_start_y(first_page);
  assert!(
    header_y < content_y,
    "header should appear in the page margin area"
  );
}

#[test]
fn inline_running_element_used_in_margin_box() {
  let html = r#"
    <html>
      <head>
        <style>
          @page {
            size: 200px 200px;
            margin: 20px;
            @top-center { content: element(header); }
          }
          h1 span { position: running(header); }
        </style>
      </head>
      <body>
        <h1><span>Inline Header</span></h1>
        <div style="height: 50px"></div>
      </body>
    </html>
  "#;

  let mut renderer = FastRender::new().unwrap();
  let dom = renderer.parse_html(html).unwrap();
  let tree = renderer
    .layout_document_for_media(&dom, 400, 400, MediaType::Print)
    .unwrap();
  let page = pages(&tree)[0];

  let mut anchor_names = Vec::new();
  fn collect(node: &FragmentNode, out: &mut Vec<String>) {
    if let FragmentContent::RunningAnchor { name, .. } = &node.content {
      out.push(name.to_string());
    }
    for child in node.children.iter() {
      collect(child, out);
    }
  }
  collect(page, &mut anchor_names);
  assert!(
    anchor_names.contains(&"header".to_string()),
    "running anchor should be captured"
  );

  assert!(
    margin_boxes_contain_text(page, "Inline Header"),
    "running header should appear in margin box"
  );
  let content = page_content(page);
  assert!(
    !collected_text_compacted(content).contains("InlineHeader"),
    "running element should not paint in normal flow"
  );
}

#[test]
fn content_descendants_use_local_coordinates() {
  let html = r#"
    <html>
      <head>
        <style>
          @page { size: 200px 400px; margin: 20px; }
          html, body { margin: 0; padding: 0; }
          #fill { height: 360px; }
        </style>
      </head>
      <body>
        <div id="fill"></div>
      </body>
    </html>
  "#;

  let mut renderer = FastRender::new().unwrap();
  let dom = renderer.parse_html(html).unwrap();
  let tree = renderer
    .layout_document_for_media(&dom, 800, 1000, MediaType::Print)
    .unwrap();
  let page_roots = pages(&tree);
  let page = page_roots[0];
  let content = page_content(page);

  let target_child = content
    .children
    .iter()
    .find(|child| (child.bounds.height() - content.bounds.height()).abs() < 0.5)
    .unwrap_or_else(|| content.children.first().expect("content child"));

  let epsilon = 0.1;
  assert!(target_child.bounds.x() >= -epsilon);
  assert!(target_child.bounds.y() >= -epsilon);
  assert!(target_child.bounds.max_x() <= content.bounds.width() + epsilon);
  assert!(target_child.bounds.max_y() <= content.bounds.height() + epsilon);
}

#[test]
fn header_repeats_across_pages() {
  let html = r#"
     <html>
       <head>
         <style>
          h1 { string-set: header content(); }
          @page {
            size: 200px 200px;
            margin: 20px;
            @top-center { content: string(header); }
          }
        </style>
      </head>
      <body>
        <h1>Title</h1>
        <div style="height: 600px"></div>
      </body>
    </html>
  "#;

  let mut renderer = FastRender::new().unwrap();
  let dom = renderer.parse_html(html).unwrap();
  let tree = renderer
    .layout_document_for_media(&dom, 400, 400, MediaType::Print)
    .unwrap();
  let page_roots = pages(&tree);

  assert!(page_roots.len() >= 2);

  for page in page_roots {
    let content_y = page_content_start_y(page);

    let header_pos =
      find_text_position_matching(page, "Title", (0.0, 0.0), &|pos| pos.1 < content_y)
        .expect("page header in margin box");
    assert!(header_pos.1 < content_y);
  }
}

#[test]
fn string_set_inside_style_containment_does_not_affect_margin_boxes() {
  let html = r#"
    <html>
      <head>
        <style>
          @page {
            size: 200px 200px;
            margin: 20px;
            @top-center { content: string(header); }
          }
          body { margin: 0; }
          h1 { margin: 0; font-size: 16px; line-height: 16px; }

          #outside { string-set: header content(); }
          #contained {
            contain: style;
            break-before: page;
          }
          #contained h1 { string-set: header content(); }
        </style>
      </head>
      <body>
        <h1 id="outside">Outside</h1>
        <div style="height: 40px"></div>
        <div id="contained">
          <h1>Inside</h1>
          <div style="height: 40px"></div>
        </div>
      </body>
    </html>
  "#;

  let mut renderer = FastRender::new().unwrap();
  let dom = renderer.parse_html(html).unwrap();
  let tree = renderer
    .layout_document_for_media(&dom, 400, 400, MediaType::Print)
    .unwrap();
  let page_roots = pages(&tree);
  assert!(page_roots.len() >= 2, "expected at least two pages");

  assert!(
    margin_boxes_contain_text(page_roots[0], "Outside"),
    "first page header should use the outside string-set assignment"
  );
  assert!(
    margin_boxes_contain_text(page_roots[1], "Outside"),
    "style-contained subtree must not update the running string outside"
  );
  assert!(
    !margin_boxes_contain_text(page_roots[1], "Inside"),
    "style-contained subtree must not expose its string-set assignment"
  );
}

#[test]
fn string_defaults_to_first_assignment_on_page() {
  let html = r#"
    <html>
      <head>
        <style>
          @page {
            size: 200px 200px;
            margin: 20px;
            @top-center { content: string(chapter); }
          }
          body { margin: 0; }
          h1 {
            string-set: chapter content();
            margin: 0;
            font-size: 16px;
            line-height: 16px;
          }
        </style>
      </head>
      <body>
        <h1>First</h1>
        <h1>Second</h1>
      </body>
    </html>
  "#;

  let mut renderer = FastRender::new().unwrap();
  let dom = renderer.parse_html(html).unwrap();
  let tree = renderer
    .layout_document_for_media(&dom, 400, 400, MediaType::Print)
    .unwrap();
  let page = pages(&tree)[0];
  let margin_texts: Vec<String> = page
    .children
    .iter()
    .skip(1)
    .map(collected_text_compacted)
    .collect();

  assert!(
    margin_texts.iter().any(|text| text.contains("First")),
    "string(chapter) should default to first assignment on the page (got {margin_texts:?})"
  );
  assert!(
    !margin_texts.iter().any(|text| text.contains("Second")),
    "string(chapter) should not default to last assignment on the page (got {margin_texts:?})"
  );
}

#[test]
fn string_first_except_suppresses_assignment_page() {
  let html = r#"
    <html>
      <head>
        <style>
          @page {
            size: 200px 200px;
            margin: 20px;
            @top-center { content: string(chapter, first-except); }
          }
          body { margin: 0; }
          h1 {
            string-set: chapter content();
            margin: 0;
            font-size: 16px;
            line-height: 16px;
          }
        </style>
      </head>
      <body>
        <h1>Chapter 1</h1>
        <div style="height: 400px"></div>
      </body>
    </html>
  "#;

  let mut renderer = FastRender::new().unwrap();
  let dom = renderer.parse_html(html).unwrap();
  let tree = renderer
    .layout_document_for_media(&dom, 400, 400, MediaType::Print)
    .unwrap();
  let page_roots = pages(&tree);

  assert!(page_roots.len() >= 2, "expected at least two pages");
  assert!(
    !margin_boxes_contain_text(page_roots[0], "Chapter 1"),
    "first-except should resolve to the empty string on the assignment page"
  );
  assert!(
    margin_boxes_contain_text(page_roots[1], "Chapter 1"),
    "first-except should use the entry value on pages without assignments"
  );
}

#[test]
fn string_start_uses_assignment_only_at_page_start() {
  let html = r#"
    <html>
      <head>
        <style>
          @page {
            size: 200px 200px;
            margin: 20px;
            @top-left { content: string(chapter, start); }
          }
          body { margin: 0; }
          h1 {
            string-set: chapter content();
            margin: 0;
            font-size: 16px;
            line-height: 16px;
          }
          .break { break-before: page; }
        </style>
      </head>
      <body>
        <h1>Chapter 1</h1>
        <div style="height: 40px"></div>
        <h1 class="break">Chapter 2</h1>
      </body>
    </html>
  "#;

  let mut renderer = FastRender::new().unwrap();
  let dom = renderer.parse_html(html).unwrap();
  let tree = renderer
    .layout_document_for_media(&dom, 400, 400, MediaType::Print)
    .unwrap();
  let page_roots = pages(&tree);

  assert!(
    page_roots.len() >= 2,
    "expected forced page break to create page 2"
  );
  let page2 = page_roots[1];
  assert!(
    margin_boxes_contain_text(page2, "Chapter 2"),
    "string(chapter, start) should pick the first assignment when it starts the page"
  );
  assert!(
    !margin_boxes_contain_text(page2, "Chapter 1"),
    "string(chapter, start) should not keep the carried value when a new assignment starts the page"
  );
}

#[test]
fn element_start_uses_entry_value_unless_element_at_page_start() {
  let html = r#"
    <html>
      <head>
        <style>
          @page {
            size: 200px 200px;
            margin: 20px;
            @top-center { content: element(header, start); }
          }
          body { margin: 0; }
          h1 {
            position: running(header);
            margin: 0;
            font-size: 16px;
            line-height: 16px;
          }
        </style>
      </head>
      <body>
        <h1>Header 1</h1>
        <div style="height: 200px"></div>
        <h1>Header 2</h1>
      </body>
    </html>
  "#;

  let mut renderer = FastRender::new().unwrap();
  let dom = renderer.parse_html(html).unwrap();
  let tree = renderer
    .layout_document_for_media(&dom, 400, 400, MediaType::Print)
    .unwrap();
  let page_roots = pages(&tree);

  assert!(page_roots.len() >= 2, "expected at least two pages");
  let page2 = page_roots[1];
  assert!(
    margin_boxes_contain_text(page2, "Header 1"),
    "element(header, start) should use entry value when the first assignment is not at page start"
  );
  assert!(
    !margin_boxes_contain_text(page2, "Header 2"),
    "element(header, start) should not pick the first on-page assignment when it is not at page start"
  );
}

#[test]
fn string_set_from_split_inline_updates_once() {
  let html = r#"
 	    <html>
 	      <head>
	        <style>
	          @page {
	            size: 1200px 200px;
	            margin: 30px;
	            @top-center { content: string(header); }
	          }
	          body { margin: 0; }
	          p { width: 120px; font-size: 16px; }
          .hdr { string-set: header content(); }
        </style>
      </head>
      <body>
        <p><span class="hdr">Very long header text that wraps across lines</span></p>
        <div style="height: 300px"></div>
      </body>
    </html>
  "#;

  let mut renderer = FastRender::new().unwrap();
  let dom = renderer.parse_html(html).unwrap();
  let tree = renderer
    .layout_document_for_media(&dom, 300, 400, MediaType::Print)
    .unwrap();
  let page_roots = pages(&tree);
  let page = page_roots.first().expect("page");
  let expected = "Very long header text that wraps across lines";

  let content_y = page_content_start_y(page);

  let mut texts = Vec::new();
  collect_text_fragments(page, (0.0, 0.0), &mut texts);
  texts.retain(|t| t.y < content_y);
  texts.sort_by(|a, b| {
    a.y
      .partial_cmp(&b.y)
      .unwrap_or(std::cmp::Ordering::Equal)
      .then(a.x.partial_cmp(&b.x).unwrap_or(std::cmp::Ordering::Equal))
  });

  let mut header_text = String::new();
  for t in texts {
    header_text.push_str(&t.text);
  }
  header_text.retain(|c| !c.is_whitespace());
  let mut expected_compacted = expected.to_string();
  expected_compacted.retain(|c| !c.is_whitespace());

  assert!(
    header_text.contains(&expected_compacted),
    "expected header to include full string-set value, got {header_text:?}"
  );
}

#[test]
fn start_vs_first() {
  let html = r#"
    <html>
      <head>
        <style>
          h1 { string-set: chapter content(); }
          @page {
            size: 200px 200px;
            margin: 20px;
            @top-left { content: string(chapter, start); }
            @top-right { content: string(chapter); }
          }
        </style>
      </head>
      <body>
        <h1>Chapter 1</h1>
        <div style="height: 250px"></div>
        <h1>Chapter 2</h1>
        <div style="height: 50px"></div>
      </body>
    </html>
  "#;

  let mut renderer = FastRender::new().unwrap();
  let dom = renderer.parse_html(html).unwrap();
  let tree = renderer
    .layout_document_for_media(&dom, 400, 400, MediaType::Print)
    .unwrap();
  let page_roots = pages(&tree);

  assert!(page_roots.len() >= 2);
  let mut target_page = None;
  let mut content_y = f32::MAX;
  for page in &page_roots {
    let candidate = *page;
    let candidate_content_y = page_content_start_y(candidate);
    let has_chapter_2_in_content =
      find_text_position_matching(candidate, "2", (0.0, 0.0), &|pos| {
        pos.1 >= candidate_content_y
      })
      .is_some();
    if has_chapter_2_in_content {
      target_page = Some(candidate);
      content_y = candidate_content_y;
      break;
    }
  }
  let page = target_page.expect("page containing Chapter 2");

  let left_pos = find_text_position_matching(page, "1", (0.0, 0.0), &|pos| pos.1 < content_y)
    .expect("start value in top-left margin box");
  let right_pos = find_text_position_matching(page, "2", (0.0, 0.0), &|pos| pos.1 < content_y)
    .expect("last value in top-right margin box");

  assert!(left_pos.1 < content_y);
  assert!(right_pos.1 < content_y);
  assert!(left_pos.0 < right_pos.0);
}

#[test]
fn running_strings_follow_page_relayout_left_right() {
  let html = r#"
    <html>
      <head>
        <style>
          @page {
            size: 220px 140px;
            margin-top: 20px;
            margin-bottom: 20px;
            margin-left: 10px;
            margin-right: 10px;
            @top-left { content: string(chapter, start); }
            @top-right { content: string(chapter); }
          }
          /* Make the left page narrower so the same paragraph wraps differently. */
          @page :left { margin-left: 70px; margin-right: 10px; }

          body { margin: 0; font-family: monospace; font-size: 10px; line-height: 10px; }
          h1 { margin: 0; string-set: chapter content(); }
          p { margin: 0; }
        </style>
      </head>
      <body>
        <h1>Chapter1</h1>
        <p>
          This paragraph is long enough to wrap differently on left vs right pages. This paragraph
          is long enough to wrap differently on left vs right pages. This paragraph is long enough
          to wrap differently on left vs right pages. This paragraph is long enough to wrap
          differently on left vs right pages. This paragraph is long enough to wrap differently on
          left vs right pages. This paragraph is long enough to wrap differently on left vs right
          pages. This paragraph is long enough to wrap differently on left vs right pages.
        </p>
        <h1>Chapter2</h1>
        <div style="height: 10px"></div>
      </body>
    </html>
  "#;

  let mut renderer = FastRender::new().unwrap();
  let dom = renderer.parse_html(html).unwrap();
  let tree = renderer
    .layout_document_for_media(&dom, 400, 400, MediaType::Print)
    .unwrap();
  let page_roots = pages(&tree);

  assert!(page_roots.len() >= 2);

  // Find the page where Chapter2 appears in the main content area.
  let mut target_page = None;
  let mut content_y = f32::MAX;
  for page in &page_roots {
    let candidate = *page;
    let candidate_content_y = page_content_start_y(candidate);
    let has_chapter_2_in_content =
      find_text_position_matching(candidate, "Chapter2", (0.0, 0.0), &|pos| {
        pos.1 >= candidate_content_y
      })
      .is_some();
    if has_chapter_2_in_content {
      target_page = Some(candidate);
      content_y = candidate_content_y;
      break;
    }
  }
  let page = target_page.expect("page containing Chapter2");

  // Chapter2 should not start at the top of the page (the paragraph continues onto the left page),
  // so the page-start running string should still be Chapter1.
  assert!(
    find_text_position_matching(page, "Chapter1", (0.0, 0.0), &|pos| pos.1 < content_y).is_some(),
    "expected string(chapter, start) to resolve to Chapter1 on the Chapter2 page; margin texts={:?}",
    page
      .children
      .iter()
      .skip(1)
      .map(collected_text_compacted)
      .collect::<Vec<_>>()
  );
  assert!(
    find_text_position_matching(page, "Chapter2", (0.0, 0.0), &|pos| pos.1 < content_y).is_some(),
    "expected string(chapter) to resolve to Chapter2 on the Chapter2 page"
  );
}

#[test]
fn running_strings_update_across_named_page_width_change() {
  let html = r#"
    <html>
      <head>
        <style>
          @page { size: 220px 140px; margin: 20px; @top-center { content: string(chapter, start); } }
          @page wide { size: 320px 140px; margin: 20px; @top-center { content: string(chapter, start); } }

          body { margin: 0; font-family: monospace; font-size: 10px; line-height: 10px; }
          h1 { margin: 0; string-set: chapter content(); }
          p { margin: 0; }

          #wide { page: wide; }
        </style>
      </head>
      <body>
        <h1>Preface</h1>
        <p>
          Preface content that fills the first page so that the named page transition forces a new
          page boundary. Preface content that fills the first page so that the named page transition
          forces a new page boundary. Preface content that fills the first page so that the named
          page transition forces a new page boundary.
        </p>
        <div id="wide">
          <h1>ChapterWide</h1>
          <div style="height: 10px"></div>
        </div>
      </body>
    </html>
  "#;

  let mut renderer = FastRender::new().unwrap();
  let dom = renderer.parse_html(html).unwrap();
  let tree = renderer
    .layout_document_for_media(&dom, 400, 400, MediaType::Print)
    .unwrap();
  let page_roots = pages(&tree);

  assert!(page_roots.len() >= 2);

  // The named page transition should force ChapterWide to start on a new page.
  let wide_page = page_roots
    .iter()
    .find(|page| find_text(*page, "ChapterWide").is_some())
    .copied()
    .expect("page containing ChapterWide");
  let content_y = page_content_start_y(wide_page);

  assert!(
    find_text_position_matching(wide_page, "ChapterWide", (0.0, 0.0), &|pos| pos.1
      >= content_y)
    .is_some(),
    "expected ChapterWide to appear in page content"
  );
  assert!(
    find_text_position_matching(wide_page, "ChapterWide", (0.0, 0.0), &|pos| pos.1
      < content_y)
    .is_some(),
    "expected running string to update at the named-page boundary"
  );
}

#[test]
fn margin_box_quotes_property_applies() {
  let html = r#"
    <html>
      <head>
        <style>
          body { quotes: "<" ">"; }
          @page {
            size: 200px 120px;
            margin: 10px;
            @top-center { content: open-quote "x" close-quote; }
          }
        </style>
      </head>
      <body>
        <div style="height: 20px"></div>
      </body>
    </html>
  "#;

  let mut renderer = FastRender::new().unwrap();
  let dom = renderer.parse_html(html).unwrap();
  let tree = renderer
    .layout_document_for_media(&dom, 400, 400, MediaType::Print)
    .unwrap();
  let page = pages(&tree)[0];

  let actual = collected_text_compacted(page);
  assert!(
    actual.contains("<x>"),
    "expected <x> in margin box, got {actual}"
  );
}

#[test]
fn margin_box_url_content_creates_replaced_fragment() {
  let html = r#"
    <html>
      <head>
        <style>
          @page {
            size: 200px 120px;
            margin: 10px;
            @top-center { content: url("data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMB/6XWcJAAAAAASUVORK5CYII="); }
          }
        </style>
      </head>
      <body>
        <div>content</div>
      </body>
    </html>
  "#;

  let mut renderer = FastRender::new().unwrap();
  let dom = renderer.parse_html(html).unwrap();
  let tree = renderer
    .layout_document_for_media(&dom, 400, 400, MediaType::Print)
    .unwrap();
  let first_page = pages(&tree)[0];

  assert!(
    find_replaced_image(first_page).is_some(),
    "expected a replaced image fragment in margin boxes"
  );
}

#[test]
fn margin_box_uses_custom_counter_style() {
  let html = r#"
    <html>
      <head>
        <style>
          @counter-style alpha2 { system: fixed 1; symbols: "A" "B" "C"; }
          @page {
            size: 200px 100px;
            margin: 10px;
            @bottom-center { content: counter(page, alpha2); }
          }
        </style>
      </head>
      <body>
        <div style="height: 120px"></div>
      </body>
    </html>
  "#;

  let mut renderer = FastRender::new().unwrap();
  let dom = renderer.parse_html(html).unwrap();
  let tree = renderer
    .layout_document_for_media(&dom, 400, 400, MediaType::Print)
    .unwrap();
  let page_roots = pages(&tree);

  assert!(
    page_roots.len() >= 2,
    "expected at least two pages for page counter test"
  );

  let first = page_roots[0];
  let second = page_roots[1];

  assert!(
    find_text(first, "A").is_some(),
    "page 1 should render counter(page) with the first symbol"
  );
  assert!(
    find_text(second, "B").is_some(),
    "page 2 should render counter(page) with the second symbol"
  );
}

#[test]
fn margin_boxes_follow_page_pseudos() {
  let html = r#"
    <html>
      <head>
        <style>
          @page { size: 200px 100px; margin: 10px; }
          @page :first { @top-center { content: "FIRST"; } }
          @page :right { @bottom-center { content: "RIGHT"; } }
          @page :left { @bottom-center { content: "LEFT"; } }
          body { margin: 0; }
        </style>
      </head>
      <body>
        <div style="height: 300px"></div>
      </body>
    </html>
  "#;

  let mut renderer = FastRender::new().unwrap();
  let dom = renderer.parse_html(html).unwrap();
  let tree = renderer
    .layout_document_for_media(&dom, 400, 400, MediaType::Print)
    .unwrap();
  let page_roots = pages(&tree);

  assert!(page_roots.len() >= 3);

  let first = page_roots[0];
  let second = page_roots[1];
  let third = page_roots[2];

  assert!(find_text(first, "FIRST").is_some());
  assert!(find_text(first, "RIGHT").is_some());
  assert!(find_text(second, "LEFT").is_some());
  assert!(find_text(third, "RIGHT").is_some());
}

#[test]
fn margin_box_inherits_body_color_and_font_size() {
  let html = r#"
    <html>
      <head>
        <style>
          body { color: rgb(200, 0, 0); font-size: 30px; }
          @page {
            size: 200px 120px;
            margin: 10px;
            @top-center { content: "X"; }
          }
        </style>
      </head>
      <body></body>
    </html>
  "#;

  let mut renderer = FastRender::new().unwrap();
  let dom = renderer.parse_html(html).unwrap();
  let tree = renderer
    .layout_document_for_media(&dom, 300, 200, MediaType::Print)
    .unwrap();
  let page = *pages(&tree).first().expect("at least one page");
  let header = find_text(page, "X").expect("margin box text");
  let style = header.get_style().expect("margin text style");

  assert_eq!(style.color, Rgba::rgb(200, 0, 0));
  assert!((style.font_size - 30.0).abs() < 0.1);
}

#[test]
fn margin_box_inherits_page_context_font_size() {
  let html = r#"
    <html>
      <head>
        <style>
          body { font-size: 10px; }
          @page {
            font-size: 30px;
            size: 200px 120px;
            margin: 10px;
            @top-center { content: "X"; }
          }
        </style>
      </head>
      <body></body>
    </html>
  "#;

  let mut renderer = FastRender::new().unwrap();
  let dom = renderer.parse_html(html).unwrap();
  let tree = renderer
    .layout_document_for_media(&dom, 300, 200, MediaType::Print)
    .unwrap();
  let page = *pages(&tree).first().expect("at least one page");
  let header = find_text(page, "X").expect("margin box text");
  let style = header.get_style().expect("margin text style");

  assert!((style.font_size - 30.0).abs() < 0.1);
}

#[test]
fn margin_box_inherits_page_context_color() {
  let html = r#"
    <html>
      <head>
        <style>
          body { color: rgb(255, 0, 0); }
          @page {
            color: rgb(0, 255, 0);
            size: 200px 120px;
            margin: 10px;
            @top-center { content: "X"; }
          }
        </style>
      </head>
      <body></body>
    </html>
  "#;

  let mut renderer = FastRender::new().unwrap();
  let dom = renderer.parse_html(html).unwrap();
  let tree = renderer
    .layout_document_for_media(&dom, 300, 200, MediaType::Print)
    .unwrap();
  let page = *pages(&tree).first().expect("at least one page");
  let header = find_text(page, "X").expect("margin box text");
  let style = header.get_style().expect("margin text style");

  assert_eq!(style.color, Rgba::rgb(0, 255, 0));
}

#[test]
fn margin_box_display_none_does_not_suppress_generation() {
  let html = r#"
    <html>
      <head>
        <style>
          @page {
            size: 200px 120px;
            margin: 10px;
            @top-center { content: "X"; display: none; }
          }
        </style>
      </head>
      <body></body>
    </html>
  "#;

  let mut renderer = FastRender::new().unwrap();
  let dom = renderer.parse_html(html).unwrap();
  let tree = renderer
    .layout_document_for_media(&dom, 300, 200, MediaType::Print)
    .unwrap();
  let page = *pages(&tree).first().expect("at least one page");

  assert!(
    find_text(page, "X").is_some(),
    "margin box should still be generated despite display:none"
  );
}

#[test]
fn margin_box_text_is_shaped() {
  let html = r#"
    <html>
      <head>
        <style>
          @page {
            size: 200px 200px;
            margin: 20px;
            @top-center { content: "Header"; }
          }
        </style>
      </head>
      <body>
        <div>Content</div>
      </body>
    </html>
  "#;

  let mut renderer = FastRender::new().unwrap();
  let dom = renderer.parse_html(html).unwrap();
  let tree = renderer
    .layout_document_for_media(&dom, 400, 400, MediaType::Print)
    .unwrap();
  let page_roots = pages(&tree);
  let page = page_roots[0];

  let header = find_text(page, "Header").expect("header margin box fragment");

  assert!(matches!(
    header.content,
    FragmentContent::Text {
      shaped: Some(ref runs),
      ..
    } if !runs.is_empty()
  ));
}

#[test]
fn margin_box_bounds_cover_all_areas() {
  let html = r#"
    <html>
      <head>
        <style>
          @page {
            size: 200px 200px;
            margin: 10px;
            @top-left-corner { background: rgb(255, 0, 0); content: ""; }
            @top-left { background: rgb(255, 32, 0); content: ""; }
            @top-center { background: rgb(255, 64, 0); content: ""; }
            @top-right { background: rgb(255, 96, 0); content: ""; }
            @top-right-corner { background: rgb(255, 128, 0); content: ""; }
            @right-top { background: rgb(255, 160, 0); content: ""; }
            @right-middle { background: rgb(255, 192, 0); content: ""; }
            @right-bottom { background: rgb(255, 224, 0); content: ""; }
            @bottom-right-corner { background: rgb(0, 255, 0); content: ""; }
            @bottom-right { background: rgb(0, 255, 32); content: ""; }
            @bottom-center { background: rgb(0, 255, 64); content: ""; }
            @bottom-left { background: rgb(0, 255, 96); content: ""; }
            @bottom-left-corner { background: rgb(0, 255, 128); content: ""; }
            @left-bottom { background: rgb(0, 0, 255); content: ""; }
            @left-middle { background: rgb(32, 0, 255); content: ""; }
            @left-top { background: rgb(64, 0, 255); content: ""; }
          }
        </style>
      </head>
      <body></body>
    </html>
  "#;

  let mut renderer = FastRender::new().unwrap();
  let dom = renderer.parse_html(html).unwrap();
  let tree = renderer
    .layout_document_for_media(&dom, 400, 400, MediaType::Print)
    .unwrap();
  let page = *pages(&tree).first().expect("at least one page");

  let expectations = vec![
    (Rgba::rgb(255, 0, 0), (0.0, 0.0, 10.0, 10.0)),
    (Rgba::rgb(255, 32, 0), (10.0, 0.0, 45.0, 10.0)),
    (Rgba::rgb(255, 64, 0), (55.0, 0.0, 90.0, 10.0)),
    (Rgba::rgb(255, 96, 0), (145.0, 0.0, 45.0, 10.0)),
    (Rgba::rgb(255, 128, 0), (190.0, 0.0, 10.0, 10.0)),
    (Rgba::rgb(255, 160, 0), (190.0, 10.0, 10.0, 45.0)),
    (Rgba::rgb(255, 192, 0), (190.0, 55.0, 10.0, 90.0)),
    (Rgba::rgb(255, 224, 0), (190.0, 145.0, 10.0, 45.0)),
    (Rgba::rgb(0, 255, 0), (190.0, 190.0, 10.0, 10.0)),
    (Rgba::rgb(0, 255, 32), (145.0, 190.0, 45.0, 10.0)),
    (Rgba::rgb(0, 255, 64), (55.0, 190.0, 90.0, 10.0)),
    (Rgba::rgb(0, 255, 96), (10.0, 190.0, 45.0, 10.0)),
    (Rgba::rgb(0, 255, 128), (0.0, 190.0, 10.0, 10.0)),
    (Rgba::rgb(0, 0, 255), (0.0, 145.0, 10.0, 45.0)),
    (Rgba::rgb(32, 0, 255), (0.0, 55.0, 10.0, 90.0)),
    (Rgba::rgb(64, 0, 255), (0.0, 10.0, 10.0, 45.0)),
  ];

  for (color, expected_bounds) in expectations {
    let fragment = find_fragment_by_background(page, color)
      .unwrap_or_else(|| panic!("missing margin box for color {:?}", color));
    assert_bounds_close(&fragment.bounds, expected_bounds);
  }
}

#[test]
fn margin_box_explicit_widths_override_auto_distribution() {
  let html = r#"
    <html>
      <head>
        <style>
          @page {
            size: 200px 200px;
            margin: 10px;
            @top-left { background: rgb(255, 0, 0); width: 20px; content: ""; }
            @top-center { background: rgb(0, 255, 0); content: ""; }
            @top-right { background: rgb(0, 0, 255); width: 40px; content: ""; }
          }
        </style>
      </head>
      <body></body>
    </html>
  "#;

  let mut renderer = FastRender::new().unwrap();
  let dom = renderer.parse_html(html).unwrap();
  let tree = renderer
    .layout_document_for_media(&dom, 400, 400, MediaType::Print)
    .unwrap();
  let page = *pages(&tree).first().expect("at least one page");

  let left = find_fragment_by_background(page, Rgba::rgb(255, 0, 0)).expect("top-left box");
  let center = find_fragment_by_background(page, Rgba::rgb(0, 255, 0)).expect("top-center box");
  let right = find_fragment_by_background(page, Rgba::rgb(0, 0, 255)).expect("top-right box");

  assert_bounds_close(&left.bounds, (10.0, 0.0, 20.0, 10.0));
  assert_bounds_close(&center.bounds, (50.0, 0.0, 100.0, 10.0));
  assert_bounds_close(&right.bounds, (150.0, 0.0, 40.0, 10.0));
}

#[test]
fn margin_box_imaginary_ac_considers_auto_side_intrinsic_sizes() {
  let html = r#"
    <html>
      <head>
        <style>
          @page {
            size: 200px 200px;
            margin: 10px;
            @top-left { background: rgb(255, 0, 0); width: 20px; content: ""; }
            @top-center { background: rgb(0, 255, 0); padding-left: 10px; content: ""; }
            @top-right { background: rgb(0, 0, 255); padding-left: 80px; content: ""; }
          }
        </style>
      </head>
      <body></body>
    </html>
  "#;

  let mut renderer = FastRender::new().unwrap();
  let dom = renderer.parse_html(html).unwrap();
  let tree = renderer
    .layout_document_for_media(&dom, 400, 400, MediaType::Print)
    .unwrap();
  let page = *pages(&tree).first().expect("at least one page");

  let left = find_fragment_by_background(page, Rgba::rgb(255, 0, 0)).expect("top-left box");
  let center = find_fragment_by_background(page, Rgba::rgb(0, 255, 0)).expect("top-center box");
  let right = find_fragment_by_background(page, Rgba::rgb(0, 0, 255)).expect("top-right box");

  // The containing block for the top margin boxes is the available width between left and right
  // margins.
  let cb_x = 10.0;
  let cb_w = 180.0;

  // CSS Page 3: resolve B (top-center) using imaginary AC, whose dimensions are double the maximum
  // of A/C. With A fixed at 20px and C auto with an intrinsic (padding) width of 80px, AC is auto
  // with max-content width 160px. Flex-fit distributes the remaining 10px between B (10px) and AC
  // (160px) proportionally.
  let max_b = 10.0;
  let max_ac = 160.0;
  let flex_space = cb_w - (max_b + max_ac);
  let used_b = max_b + flex_space * max_b / (max_b + max_ac);
  let used_c = (cb_w - used_b) / 2.0;
  let b_x = cb_x + (cb_w - used_b) / 2.0;
  let c_x = cb_x + cb_w - used_c;

  assert_bounds_close(&left.bounds, (10.0, 0.0, 20.0, 10.0));
  assert_bounds_close(&center.bounds, (b_x, 0.0, used_b, 10.0));
  assert_bounds_close(&right.bounds, (c_x, 0.0, used_c, 10.0));
}

#[test]
fn margin_box_page_counters_page_and_pages() {
  let html = r#"
    <html>
      <head>
        <style>
          @page {
            size: 200px 100px;
            margin: 10px;
            @bottom-center { content: "Page " counter(page) " / " counter(pages); }
          }
        </style>
      </head>
      <body>
        <div style="height: 150px"></div>
      </body>
    </html>
  "#;

  let mut renderer = FastRender::new().unwrap();
  let dom = renderer.parse_html(html).unwrap();
  let tree = renderer
    .layout_document_for_media(&dom, 400, 400, MediaType::Print)
    .unwrap();
  let page_roots = pages(&tree);
  assert!(page_roots.len() >= 2);

  let page_count = page_roots.len();
  for (idx, page) in page_roots.iter().enumerate() {
    let expected = format!("Page{} / {}", idx + 1, page_count)
      .chars()
      .filter(|c| !c.is_whitespace())
      .collect::<String>();
    let actual = collected_text_compacted(page);
    assert!(
      actual.contains(&expected),
      "missing page counter text on page {}",
      idx + 1
    );
  }
}

#[test]
fn paginated_pages_are_stacked_vertically() {
  let html = r#"
    <html>
      <head>
        <style>
          @page { size: 100px 120px; margin: 0; }
        </style>
      </head>
      <body>
        <div style="height: 250px"></div>
      </body>
    </html>
  "#;

  let mut renderer = FastRender::new().unwrap();
  let dom = renderer.parse_html(html).unwrap();
  let tree = renderer
    .layout_document_for_media(&dom, 200, 200, MediaType::Print)
    .unwrap();
  let page_roots = pages(&tree);

  assert!(page_roots.len() >= 2);
  assert!(page_roots[1].bounds.y() > page_roots[0].bounds.y());
  assert!(
    page_roots[1].bounds.y() - page_roots[0].bounds.y() >= page_roots[0].bounds.height() - 0.1
  );
}

#[test]
fn page_stacking_can_be_disabled() {
  let html = r#"
    <html>
      <head>
        <style>
          @page { size: 100px 120px; margin: 0; }
        </style>
      </head>
      <body>
        <div style="height: 250px"></div>
      </body>
    </html>
  "#;

  let mut renderer = FastRender::new().unwrap();
  let dom = renderer.parse_html(html).unwrap();
  let options = LayoutDocumentOptions::new().with_page_stacking(PageStacking::Untranslated);
  let tree = renderer
    .layout_document_for_media_with_options(&dom, 200, 200, MediaType::Print, options, None)
    .unwrap();
  let page_roots = pages(&tree);

  assert!(page_roots.len() >= 2);
  assert!((page_roots[0].bounds.y() - page_roots[1].bounds.y()).abs() < 0.01);
}

fn find_text_position(node: &FragmentNode, needle: &str, origin: (f32, f32)) -> Option<(f32, f32)> {
  let current = (origin.0 + node.bounds.x(), origin.1 + node.bounds.y());
  if let FragmentContent::Text { text, .. } = &node.content {
    if text.contains(needle) {
      return Some(current);
    }
  }
  for child in node.children.iter() {
    if let Some(pos) = find_text_position(child, needle, current) {
      return Some(pos);
    }
  }
  None
}

fn find_text_position_matching<F>(
  node: &FragmentNode,
  needle: &str,
  origin: (f32, f32),
  predicate: &F,
) -> Option<(f32, f32)>
where
  F: Fn((f32, f32)) -> bool,
{
  let current = (origin.0 + node.bounds.x(), origin.1 + node.bounds.y());
  if let FragmentContent::Text { text, .. } = &node.content {
    if text.contains(needle) && predicate(current) {
      return Some(current);
    }
  }
  for child in node.children.iter() {
    if let Some(pos) = find_text_position_matching(child, needle, current, predicate) {
      return Some(pos);
    }
  }
  None
}

#[test]
fn fixed_headers_repeat_per_page() {
  let html = r#"
    <html>
      <head>
        <style>
          @page { size: 200px 200px; margin: 0; }
          body { margin: 0; }
          .header { position: fixed; top: 0; left: 0; height: 20px; }
          .spacer { height: 500px; }
        </style>
      </head>
      <body>
        <div class="header">FixedHeader</div>
        <div class="spacer"></div>
      </body>
    </html>
  "#;

  let mut renderer = FastRender::new().unwrap();
  let dom = renderer.parse_html(html).unwrap();
  let tree = renderer
    .layout_document_for_media(&dom, 200, 200, MediaType::Print)
    .unwrap();
  let page_roots = pages(&tree);

  assert!(page_roots.len() >= 2);

  let first_header_y = find_text(page_roots[0], "FixedHeader")
    .expect("header on first page")
    .bounds
    .y();

  for (index, page) in page_roots.iter().enumerate() {
    let header = find_text(page, "FixedHeader")
      .unwrap_or_else(|| panic!("missing header on page {}", index + 1));
    assert!(
      (header.bounds.y() - first_header_y).abs() < 0.1,
      "header should be consistently positioned across pages"
    );
  }
}

#[test]
fn abspos_break_before_after_do_not_force_page_break() {
  let html = r#"
    <html>
      <head>
        <style>
          @page { size: 200px 200px; margin: 0; }
          body { margin: 0; }
          .blk { height: 50px; margin: 0; }
          #abs {
            position: absolute;
            top: 0;
            left: 0;
            break-before: page;
            break-after: page;
          }
        </style>
      </head>
      <body>
        <div class="blk">MAIN_A</div>
        <div id="abs">ABS</div>
        <div class="blk">MAIN_B</div>
      </body>
    </html>
  "#;

  let mut renderer = FastRender::new().unwrap();
  let dom = renderer.parse_html(html).unwrap();
  let tree = renderer
    .layout_document_for_media(&dom, 200, 200, MediaType::Print)
    .unwrap();
  let page_roots = pages(&tree);

  assert_eq!(
    page_roots.len(),
    1,
    "break-before/after must not apply to absolutely-positioned boxes"
  );
  assert!(
    find_text(page_roots[0], "MAIN_A").is_some(),
    "MAIN_A should be on page 1"
  );
  assert!(
    find_text(page_roots[0], "MAIN_B").is_some(),
    "MAIN_B should be on page 1"
  );
}

#[test]
fn forced_break_inside_abspos_does_not_paginate_main_flow() {
  let html = r#"
    <html>
      <head>
        <style>
          @page { size: 200px 200px; margin: 0; }
          body { margin: 0; }
          .blk { height: 20px; margin: 0; }
          #abs {
            position: absolute;
            top: 0;
            left: 0;
            width: 200px;
          }
          .abs-item { height: 20px; margin: 0; }
          #abs1 { break-after: page; }
        </style>
      </head>
      <body>
        <div class="blk">MAIN_A</div>
        <div class="blk">MAIN_B</div>
        <div id="abs">
          <div id="abs1" class="abs-item">ABS1</div>
          <div id="abs2" class="abs-item">ABS2</div>
        </div>
      </body>
    </html>
  "#;

  let mut renderer = FastRender::new().unwrap();
  let dom = renderer.parse_html(html).unwrap();
  let tree = renderer
    .layout_document_for_media(&dom, 200, 200, MediaType::Print)
    .unwrap();
  let page_roots = pages(&tree);

  assert_eq!(
    page_roots.len(),
    2,
    "forced breaks inside abspos should create continuation pages without splitting the main flow"
  );
  assert!(
    find_text(page_roots[0], "MAIN_A").is_some(),
    "MAIN_A should be on page 1"
  );
  assert!(
    find_text(page_roots[0], "MAIN_B").is_some(),
    "MAIN_B should be on page 1"
  );
  assert!(
    find_text(page_roots[0], "ABS2").is_none(),
    "ABS2 should not appear on page 1"
  );
  assert!(
    find_text(page_roots[1], "ABS2").is_some(),
    "ABS2 should appear on page 2"
  );
  assert!(
    find_text(page_roots[1], "MAIN_A").is_none(),
    "MAIN_A should not be on page 2"
  );
  assert!(
    find_text(page_roots[1], "MAIN_B").is_none(),
    "MAIN_B should not be on page 2"
  );
}

#[test]
fn multicol_columns_continue_across_pages() {
  let html = r#"
    <html>
      <head>
        <style>
          @page { size: 200px 200px; margin: 0; }
          body { margin: 0; }
          .multi { column-count: 2; column-gap: 0; }
          .section { height: 150px; }
        </style>
      </head>
      <body>
        <div class="multi">
          <div class="section">One</div>
          <div class="section">Two</div>
          <div class="section">Three</div>
        </div>
      </body>
    </html>
  "#;

  let mut renderer = FastRender::new().unwrap();
  let dom = renderer.parse_html(html).unwrap();
  let options = LayoutDocumentOptions::new().with_page_stacking(PageStacking::Untranslated);
  let tree = renderer
    .layout_document_for_media_with_options(&dom, 400, 400, MediaType::Print, options, None)
    .unwrap();
  let page_roots = pages(&tree);

  let first = page_roots[0];
  let second = page_roots[1];

  let pos_one = find_text_position(first, "One", (0.0, 0.0)).expect("first column");
  let pos_two = find_text_position(first, "Two", (0.0, 0.0)).expect("second column");
  assert!(find_text_position(first, "Three", (0.0, 0.0)).is_none());

  assert!(
    pos_two.0 > pos_one.0,
    "second column should be to the right"
  );
  assert!(
    pos_one.1 < 200.0 && pos_two.1 < 200.0,
    "page 1 content fits height"
  );

  let pos_three = find_text_position(second, "Three", (0.0, 0.0)).expect("continued content");
  assert!(
    pos_three.1 < 20.0,
    "next column set starts at top of next page"
  );
  assert!(find_text_position(second, "One", (0.0, 0.0)).is_none());
}

#[test]
fn multicol_break_after_page_forces_new_page() {
  let html = r#"
    <html>
      <head>
        <style>
          @page { size: 200px 200px; margin: 0; }
          body { margin: 0; }
          .multi { column-count: 2; column-gap: 0; }
          #a { height: 150px; }
          #b { height: 10px; break-after: page; }
          #c { height: 150px; }
        </style>
      </head>
      <body>
        <div class="multi">
          <div id="a">A</div>
          <div id="b">B</div>
          <div id="c">C</div>
        </div>
      </body>
    </html>
  "#;

  let mut renderer = FastRender::new().unwrap();
  let dom = renderer.parse_html(html).unwrap();
  let tree = renderer
    .layout_document_for_media(&dom, 200, 200, MediaType::Print)
    .unwrap();
  let page_roots = pages(&tree);

  assert_eq!(
    page_roots.len(),
    2,
    "expected break-after: page inside multicol to force a new page; got {} pages",
    page_roots.len()
  );
  assert!(find_text(page_roots[0], "C").is_none(), "C should not appear on page 1");
  assert!(find_text(page_roots[1], "C").is_some(), "C should appear on page 2");
}

#[test]
fn multicol_break_after_always_only_forces_column_break() {
  let html = r#"
    <html>
      <head>
        <style>
          @page { size: 200px 200px; margin: 0; }
          body { margin: 0; }
          .multi { column-count: 2; column-gap: 0; }
          #a { height: 150px; }
          #b { height: 10px; break-after: always; }
          #c { height: 150px; }
        </style>
      </head>
      <body>
        <div class="multi">
          <div id="a">A</div>
          <div id="b">B</div>
          <div id="c">C</div>
        </div>
      </body>
    </html>
  "#;

  let mut renderer = FastRender::new().unwrap();
  let dom = renderer.parse_html(html).unwrap();
  let tree = renderer
    .layout_document_for_media(&dom, 200, 200, MediaType::Print)
    .unwrap();
  let page_roots = pages(&tree);

  assert_eq!(
    page_roots.len(),
    1,
    "expected break-after: always inside multicol to stay within the same page; got {} pages",
    page_roots.len()
  );
  let (left, right) = count_text_fragments_by_column(page_roots[0], "C");
  assert!(
    left == 0 && right > 0,
    "expected C to appear in the second column after a column break; got left={left} right={right}"
  );
}

#[test]
fn multicol_columns_align_to_page_boundary_when_offset_within_page() {
  let html = r#"
    <html>
      <head>
        <style>
          @page { size: 200px 200px; margin: 0; }
          body { margin: 0; }
          .spacer { height: 120px; }
          .multi { column-count: 2; column-gap: 0; }
          .blk { height: 20px; margin: 0; }
          .break { break-after: column; }
        </style>
      </head>
      <body>
        <div class="spacer"></div>
        <div class="multi">
          <div class="blk break">One</div>
          <div class="blk break">Two</div>
          <div class="blk">Three</div>
        </div>
      </body>
    </html>
  "#;

  let mut renderer = FastRender::new().unwrap();
  let dom = renderer.parse_html(html).unwrap();
  let options = LayoutDocumentOptions::new().with_page_stacking(PageStacking::Untranslated);
  let tree = renderer
    .layout_document_for_media_with_options(&dom, 400, 400, MediaType::Print, options, None)
    .unwrap();
  let page_roots = pages(&tree);

  assert_eq!(
    page_roots.len(),
    2,
    "expected exactly two pages, got {}",
    page_roots.len()
  );

  let first = page_roots[0];
  let second = page_roots[1];

  assert!(find_text_position(first, "One", (0.0, 0.0)).is_some());
  assert!(find_text_position(first, "Two", (0.0, 0.0)).is_some());
  assert!(find_text_position(first, "Three", (0.0, 0.0)).is_none());

  assert!(find_text_position(second, "One", (0.0, 0.0)).is_none());
  assert!(find_text_position(second, "Two", (0.0, 0.0)).is_none());
  let pos_three = find_text_position(second, "Three", (0.0, 0.0)).expect("Three on page 2");
  assert!(
    pos_three.1 < 20.0,
    "next column set should align to the top of the next page, got y={}",
    pos_three.1
  );
}

#[test]
fn multicol_break_after_page_promotes_to_next_column_set_with_offset_within_page() {
  let html = r#"
    <html>
      <head>
        <style>
          @page { size: 200px 200px; margin: 0; }
          body { margin: 0; }
          .spacer { height: 120px; }
          .multi { column-count: 2; column-gap: 0; }
          .a, .b { height: 60px; margin: 0; }
          .a { break-after: page; }
        </style>
      </head>
      <body>
        <div class="spacer"></div>
        <div class="multi">
          <div class="a">A</div>
          <div class="b">B</div>
        </div>
      </body>
    </html>
  "#;

  let mut renderer = FastRender::new().unwrap();
  let dom = renderer.parse_html(html).unwrap();
  let options = LayoutDocumentOptions::new().with_page_stacking(PageStacking::Untranslated);
  let tree = renderer
    .layout_document_for_media_with_options(&dom, 400, 400, MediaType::Print, options, None)
    .unwrap();
  let page_roots = pages(&tree);

  assert_eq!(
    page_roots.len(),
    2,
    "expected forced break to create exactly two pages"
  );
  let page1 = page_roots[0];
  let page2 = page_roots[1];

  assert!(find_text(page1, "A").is_some());
  assert!(find_text(page1, "B").is_none());
  assert!(find_text(page2, "A").is_none());
  assert!(find_text(page2, "B").is_some());

  let pos_b = find_text_position(page2, "B", (0.0, 0.0)).expect("B position");
  assert!(
    pos_b.1 <= page_content_start_y(page2) + 1.0,
    "B should start near the top of the next column set/page (y={}, content_start={})",
    pos_b.1,
    page_content_start_y(page2)
  );
}

#[test]
fn multicol_break_after_page_promotes_to_next_column_set() {
  let html = r#"
    <html>
      <head>
        <style>
          @page { size: 200px 140px; margin: 20px; }
          body { margin: 0; }
          .multi { column-count: 2; column-gap: 0; }
          .a, .b { height: 60px; margin: 0; }
          .a { break-after: page; }
        </style>
      </head>
      <body>
        <div class="multi">
          <div class="a">A</div>
          <div class="b">B</div>
        </div>
      </body>
    </html>
  "#;

  let mut renderer = FastRender::new().unwrap();
  let dom = renderer.parse_html(html).unwrap();
  let options = LayoutDocumentOptions::new().with_page_stacking(PageStacking::Untranslated);
  let tree = renderer
    .layout_document_for_media_with_options(&dom, 400, 400, MediaType::Print, options, None)
    .unwrap();
  let page_roots = pages(&tree);

  assert_eq!(
    page_roots.len(),
    2,
    "expected forced break to create exactly two pages"
  );
  let page1 = page_roots[0];
  let page2 = page_roots[1];

  assert!(find_text_eq(page1, "A").is_some());
  assert!(find_text_eq(page1, "B").is_none());
  assert!(find_text(page2, "A").is_none());
  assert!(find_text(page2, "B").is_some());

  let pos_b = find_text_position(page2, "B", (0.0, 0.0)).expect("B position");
  assert!(
    pos_b.1 <= page_content_start_y(page2) + 1.0,
    "B should start near the top of the next column set/page (y={}, content_start={})",
    pos_b.1,
    page_content_start_y(page2)
  );
}

#[test]
fn multicol_break_after_page_promotes_to_next_column_set_with_three_columns() {
  let html = r#"
    <html>
      <head>
        <style>
          @page { size: 200px 140px; margin: 20px; }
          body { margin: 0; }
          .multi { column-count: 3; column-gap: 0; }
          .a, .b { height: 60px; margin: 0; }
          .a { break-after: page; }
        </style>
      </head>
      <body>
        <div class="multi">
          <div class="a">A</div>
          <div class="b">B</div>
        </div>
      </body>
    </html>
  "#;

  let mut renderer = FastRender::new().unwrap();
  let dom = renderer.parse_html(html).unwrap();
  let options = LayoutDocumentOptions::new().with_page_stacking(PageStacking::Untranslated);
  let tree = renderer
    .layout_document_for_media_with_options(&dom, 400, 400, MediaType::Print, options, None)
    .unwrap();
  let page_roots = pages(&tree);

  assert_eq!(
    page_roots.len(),
    2,
    "expected forced break to create exactly two pages"
  );
  let page1 = page_roots[0];
  let page2 = page_roots[1];

  assert!(find_text(page1, "A").is_some());
  assert!(find_text(page1, "B").is_none());
  assert!(find_text(page2, "A").is_none());
  assert!(find_text(page2, "B").is_some());

  let pos_b = find_text_position(page2, "B", (0.0, 0.0)).expect("B position");
  assert!(
    pos_b.1 <= page_content_start_y(page2) + 1.0,
    "B should start near the top of the next column set/page (y={}, content_start={})",
    pos_b.1,
    page_content_start_y(page2)
  );
}

#[test]
fn multicol_break_before_page_promotes_to_next_column_set() {
  let html = r#"
    <html>
      <head>
        <style>
          @page { size: 200px 140px; margin: 20px; }
          body { margin: 0; }
          .multi { column-count: 2; column-gap: 0; }
          .a, .b { height: 60px; margin: 0; }
          .b { break-before: page; }
        </style>
      </head>
      <body>
        <div class="multi">
          <div class="a">A</div>
          <div class="b">B</div>
        </div>
      </body>
    </html>
  "#;

  let mut renderer = FastRender::new().unwrap();
  let dom = renderer.parse_html(html).unwrap();
  let options = LayoutDocumentOptions::new().with_page_stacking(PageStacking::Untranslated);
  let tree = renderer
    .layout_document_for_media_with_options(&dom, 400, 400, MediaType::Print, options, None)
    .unwrap();
  let page_roots = pages(&tree);

  assert_eq!(
    page_roots.len(),
    2,
    "expected forced break to create exactly two pages"
  );
  let page1 = page_roots[0];
  let page2 = page_roots[1];

  assert!(find_text(page1, "A").is_some());
  assert!(find_text(page1, "B").is_none());
  assert!(find_text(page2, "A").is_none());
  assert!(find_text(page2, "B").is_some());

  let pos_b = find_text_position(page2, "B", (0.0, 0.0)).expect("B position");
  assert!(
    pos_b.1 <= page_content_start_y(page2) + 1.0,
    "B should start near the top of the next column set/page (y={}, content_start={})",
    pos_b.1,
    page_content_start_y(page2)
  );
}

#[test]
fn multicol_break_after_right_inserts_blank_page() {
  let html = r#"
    <html>
      <head>
        <style>
          @page { size: 200px 140px; margin: 20px; }
          @page :blank { @top-center { content: "Blank"; } }
          body { margin: 0; }
          .multi { column-count: 2; column-gap: 0; }
          .a, .b { height: 60px; margin: 0; }
          .a { break-after: right; }
        </style>
      </head>
      <body>
        <div class="multi">
          <div class="a">A</div>
          <div class="b">B</div>
        </div>
      </body>
    </html>
  "#;

  let mut renderer = FastRender::new().unwrap();
  let dom = renderer.parse_html(html).unwrap();
  let options = LayoutDocumentOptions::new().with_page_stacking(PageStacking::Untranslated);
  let tree = renderer
    .layout_document_for_media_with_options(&dom, 400, 400, MediaType::Print, options, None)
    .unwrap();
  let page_roots = pages(&tree);

  assert_eq!(
    page_roots.len(),
    3,
    "expected right-side break to insert a blank page"
  );
  let page1 = page_roots[0];
  let page2 = page_roots[1];
  let page3 = page_roots[2];

  assert!(find_text(page1, "A").is_some());
  assert!(find_text(page1, "B").is_none());

  assert!(
    margin_boxes_contain_text(page2, "Blank"),
    "blank page should use the :blank page rule"
  );
  assert!(find_text_eq(page2, "A").is_none());
  assert!(find_text_eq(page2, "B").is_none());

  assert!(find_text_eq(page3, "B").is_some());
  let pos_b = find_text_position(page3, "B", (0.0, 0.0)).expect("B position");
  assert!(
    pos_b.1 <= page_content_start_y(page3) + 1.0,
    "B should start near the top of the next column set/page (y={}, content_start={})",
    pos_b.1,
    page_content_start_y(page3)
  );
}

#[test]
fn multicol_break_before_right_inserts_blank_page() {
  let html = r#"
    <html>
      <head>
        <style>
          @page { size: 200px 140px; margin: 20px; }
          @page :blank { @top-center { content: "Blank"; } }
          body { margin: 0; }
          .multi { column-count: 2; column-gap: 0; }
          .a, .b { height: 60px; margin: 0; }
          .b { break-before: right; }
        </style>
      </head>
      <body>
        <div class="multi">
          <div class="a">A</div>
          <div class="b">B</div>
        </div>
      </body>
    </html>
  "#;

  let mut renderer = FastRender::new().unwrap();
  let dom = renderer.parse_html(html).unwrap();
  let options = LayoutDocumentOptions::new().with_page_stacking(PageStacking::Untranslated);
  let tree = renderer
    .layout_document_for_media_with_options(&dom, 400, 400, MediaType::Print, options, None)
    .unwrap();
  let page_roots = pages(&tree);

  assert_eq!(
    page_roots.len(),
    3,
    "expected right-side break to insert a blank page"
  );
  let page1 = page_roots[0];
  let page2 = page_roots[1];
  let page3 = page_roots[2];

  assert!(find_text_eq(page1, "A").is_some());
  assert!(find_text_eq(page1, "B").is_none());

  assert!(
    margin_boxes_contain_text(page2, "Blank"),
    "blank page should use the :blank page rule"
  );
  assert!(find_text_eq(page2, "A").is_none());
  assert!(find_text_eq(page2, "B").is_none());

  assert!(find_text_eq(page3, "B").is_some());
  let pos_b = find_text_position(page3, "B", (0.0, 0.0)).expect("B position");
  assert!(
    pos_b.1 <= page_content_start_y(page3) + 1.0,
    "B should start near the top of the next column set/page (y={}, content_start={})",
    pos_b.1,
    page_content_start_y(page3)
  );
}

#[test]
fn multicol_break_after_left_inserts_blank_page_when_break_occurs_on_left_page() {
  let html = r#"
    <html>
      <head>
        <style>
          @page { size: 200px 140px; margin: 20px; }
          @page :blank { @top-center { content: "Blank"; } }
          body { margin: 0; }
          .multi { column-count: 2; column-gap: 0; }
          .pre { height: 100px; margin: 0; }
          .a, .b { height: 60px; margin: 0; }
          .a { break-after: left; }
        </style>
      </head>
      <body>
        <div class="multi">
          <div class="pre"></div>
          <div class="pre"></div>
          <div class="a">A</div>
          <div class="b">B</div>
        </div>
      </body>
    </html>
  "#;

  let mut renderer = FastRender::new().unwrap();
  let dom = renderer.parse_html(html).unwrap();
  let options = LayoutDocumentOptions::new().with_page_stacking(PageStacking::Untranslated);
  let tree = renderer
    .layout_document_for_media_with_options(&dom, 400, 400, MediaType::Print, options, None)
    .unwrap();
  let page_roots = pages(&tree);

  assert_eq!(
    page_roots.len(),
    4,
    "expected left-side break on a left page to insert a blank page"
  );
  let page1 = page_roots[0];
  let page2 = page_roots[1];
  let page3 = page_roots[2];
  let page4 = page_roots[3];

  assert!(find_text(page1, "A").is_none());
  assert!(find_text(page1, "B").is_none());

  assert!(find_text(page2, "A").is_some());
  assert!(find_text(page2, "B").is_none());

  assert!(
    margin_boxes_contain_text(page3, "Blank"),
    "blank page should use the :blank page rule"
  );
  assert!(find_text(page3, "A").is_none());
  assert!(find_text_eq(page3, "B").is_none());

  assert!(find_text(page4, "B").is_some());
  let pos_b = find_text_position(page4, "B", (0.0, 0.0)).expect("B position");
  assert!(
    pos_b.1 <= page_content_start_y(page4) + 1.0,
    "B should start near the top of the next column set/page (y={}, content_start={})",
    pos_b.1,
    page_content_start_y(page4)
  );
}

#[test]
fn multicol_break_before_left_inserts_blank_page_when_break_starts_on_right_page() {
  let html = r#"
    <html>
      <head>
        <style>
          @page { size: 200px 140px; margin: 20px; }
          @page :blank { @top-center { content: "Blank"; } }
          body { margin: 0; }
          .multi { column-count: 2; column-gap: 0; }
          .pre { height: 100px; margin: 0; }
          .b { height: 60px; margin: 0; break-before: left; }
        </style>
      </head>
      <body>
        <div class="multi">
          <div class="pre"></div>
          <div class="pre"></div>
          <div class="pre"></div>
          <div class="pre"></div>
          <div class="b">B</div>
        </div>
      </body>
    </html>
  "#;

  let mut renderer = FastRender::new().unwrap();
  let dom = renderer.parse_html(html).unwrap();
  let options = LayoutDocumentOptions::new().with_page_stacking(PageStacking::Untranslated);
  let tree = renderer
    .layout_document_for_media_with_options(&dom, 400, 400, MediaType::Print, options, None)
    .unwrap();
  let page_roots = pages(&tree);

  assert_eq!(
    page_roots.len(),
    4,
    "expected left-side break-before on a right page to insert a blank page"
  );
  let page3 = page_roots[2];
  let page4 = page_roots[3];

  assert!(
    margin_boxes_contain_text(page3, "Blank"),
    "blank page should use the :blank page rule"
  );
  assert!(find_text_eq(page3, "B").is_none());

  assert!(find_text(page4, "B").is_some());
  let pos_b = find_text_position(page4, "B", (0.0, 0.0)).expect("B position");
  assert!(
    pos_b.1 <= page_content_start_y(page4) + 1.0,
    "B should start near the top of the next column set/page (y={}, content_start={})",
    pos_b.1,
    page_content_start_y(page4)
  );
}

#[test]
fn multicol_break_after_recto_uses_root_page_progression() {
  let html = r#"
    <html>
      <head>
        <style>
          html { direction: rtl; }
          @page { size: 200px 140px; margin: 20px; }
          @page :blank { @top-center { content: "Blank"; } }
          body { margin: 0; }
          .multi { column-count: 2; column-gap: 0; direction: ltr; }
          .a, .b { height: 60px; margin: 0; }
          .a { break-after: recto; }
        </style>
      </head>
      <body>
        <div class="multi">
          <div class="a">A</div>
          <div class="b">B</div>
        </div>
      </body>
    </html>
  "#;

  let mut renderer = FastRender::new().unwrap();
  let dom = renderer.parse_html(html).unwrap();
  let options = LayoutDocumentOptions::new().with_page_stacking(PageStacking::Untranslated);
  let tree = renderer
    .layout_document_for_media_with_options(&dom, 400, 400, MediaType::Print, options, None)
    .unwrap();
  let page_roots = pages(&tree);

  assert_eq!(
    page_roots.len(),
    3,
    "expected recto break to insert a blank page based on root page progression"
  );
  let page1 = page_roots[0];
  let page2 = page_roots[1];
  let page3 = page_roots[2];

  assert!(find_text_eq(page1, "A").is_some());
  assert!(find_text_eq(page1, "B").is_none());

  assert!(
    margin_boxes_contain_text(page2, "Blank"),
    "blank page should use the :blank page rule"
  );
  assert!(find_text_eq(page2, "A").is_none());
  assert!(find_text_eq(page2, "B").is_none());

  assert!(find_text_eq(page3, "B").is_some());
  let pos_b = find_text_position(page3, "B", (0.0, 0.0)).expect("B position");
  assert!(
    pos_b.1 <= page_content_start_y(page3) + 1.0,
    "B should start near the top of the next column set/page (y={}, content_start={})",
    pos_b.1,
    page_content_start_y(page3)
  );
}

#[test]
fn multicol_break_before_recto_uses_root_page_progression() {
  let html = r#"
    <html>
      <head>
        <style>
          html { direction: rtl; }
          @page { size: 200px 140px; margin: 20px; }
          @page :blank { @top-center { content: "Blank"; } }
          body { margin: 0; }
          .multi { column-count: 2; column-gap: 0; direction: ltr; }
          .a, .b { height: 60px; margin: 0; }
          .b { break-before: recto; }
        </style>
      </head>
      <body>
        <div class="multi">
          <div class="a">A</div>
          <div class="b">B</div>
        </div>
      </body>
    </html>
  "#;

  let mut renderer = FastRender::new().unwrap();
  let dom = renderer.parse_html(html).unwrap();
  let options = LayoutDocumentOptions::new().with_page_stacking(PageStacking::Untranslated);
  let tree = renderer
    .layout_document_for_media_with_options(&dom, 400, 400, MediaType::Print, options, None)
    .unwrap();
  let page_roots = pages(&tree);

  assert_eq!(
    page_roots.len(),
    3,
    "expected recto break to insert a blank page based on root page progression"
  );
  let page1 = page_roots[0];
  let page2 = page_roots[1];
  let page3 = page_roots[2];

  assert!(find_text(page1, "A").is_some());
  assert!(find_text(page1, "B").is_none());

  assert!(
    margin_boxes_contain_text(page2, "Blank"),
    "blank page should use the :blank page rule"
  );
  assert!(find_text(page2, "A").is_none());
  assert!(find_text_eq(page2, "B").is_none());

  assert!(find_text(page3, "B").is_some());
  let pos_b = find_text_position(page3, "B", (0.0, 0.0)).expect("B position");
  assert!(
    pos_b.1 <= page_content_start_y(page3) + 1.0,
    "B should start near the top of the next column set/page (y={}, content_start={})",
    pos_b.1,
    page_content_start_y(page3)
  );
}

#[test]
fn multicol_break_after_verso_uses_root_page_progression() {
  let html = r#"
    <html>
      <head>
        <style>
          html { direction: rtl; }
          @page { size: 200px 140px; margin: 20px; }
          body { margin: 0; }
          .multi { column-count: 2; column-gap: 0; direction: ltr; }
          .a, .b { height: 60px; margin: 0; }
          .a { break-after: verso; }
        </style>
      </head>
      <body>
        <div class="multi">
          <div class="a">A</div>
          <div class="b">B</div>
        </div>
      </body>
    </html>
  "#;

  let mut renderer = FastRender::new().unwrap();
  let dom = renderer.parse_html(html).unwrap();
  let options = LayoutDocumentOptions::new().with_page_stacking(PageStacking::Untranslated);
  let tree = renderer
    .layout_document_for_media_with_options(&dom, 400, 400, MediaType::Print, options, None)
    .unwrap();
  let page_roots = pages(&tree);

  assert_eq!(
    page_roots.len(),
    2,
    "expected verso break to start content on the next page without inserting a blank page"
  );
  let page1 = page_roots[0];
  let page2 = page_roots[1];

  assert!(find_text(page1, "A").is_some());
  assert!(find_text(page1, "B").is_none());
  assert!(find_text(page2, "A").is_none());
  assert!(find_text(page2, "B").is_some());

  let pos_b = find_text_position(page2, "B", (0.0, 0.0)).expect("B position");
  assert!(
    pos_b.1 <= page_content_start_y(page2) + 1.0,
    "B should start near the top of the next column set/page (y={}, content_start={})",
    pos_b.1,
    page_content_start_y(page2)
  );
}

#[test]
fn multicol_break_before_verso_uses_root_page_progression() {
  let html = r#"
    <html>
      <head>
        <style>
          html { direction: rtl; }
          @page { size: 200px 140px; margin: 20px; }
          body { margin: 0; }
          .multi { column-count: 2; column-gap: 0; direction: ltr; }
          .a, .b { height: 60px; margin: 0; }
          .b { break-before: verso; }
        </style>
      </head>
      <body>
        <div class="multi">
          <div class="a">A</div>
          <div class="b">B</div>
        </div>
      </body>
    </html>
  "#;

  let mut renderer = FastRender::new().unwrap();
  let dom = renderer.parse_html(html).unwrap();
  let options = LayoutDocumentOptions::new().with_page_stacking(PageStacking::Untranslated);
  let tree = renderer
    .layout_document_for_media_with_options(&dom, 400, 400, MediaType::Print, options, None)
    .unwrap();
  let page_roots = pages(&tree);

  assert_eq!(
    page_roots.len(),
    2,
    "expected verso break-before to start content on the next page without inserting a blank page"
  );
  let page1 = page_roots[0];
  let page2 = page_roots[1];

  assert!(find_text(page1, "A").is_some());
  assert!(find_text(page1, "B").is_none());
  assert!(find_text(page2, "A").is_none());
  assert!(find_text(page2, "B").is_some());

  let pos_b = find_text_position(page2, "B", (0.0, 0.0)).expect("B position");
  assert!(
    pos_b.1 <= page_content_start_y(page2) + 1.0,
    "B should start near the top of the next column set/page (y={}, content_start={})",
    pos_b.1,
    page_content_start_y(page2)
  );
}

fn multicol_balance_html(column_fill: &str) -> String {
  let mut lines = String::new();
  for idx in 0..36 {
    if idx == 11 {
      lines.push_str(r#"<div class="line break">x</div>"#);
    } else {
      lines.push_str(r#"<div class="line">x</div>"#);
    }
  }

  format!(
    r#"
    <html>
      <head>
        <style>
          @page {{ size: 200px 100px; margin: 0; }}
          body {{ margin: 0; font-size: 10px; line-height: 10px; }}
          .multi {{ column-count: 2; column-gap: 0; column-fill: {column_fill}; }}
          .line {{ height: 10px; margin: 0; padding: 0; }}
          .break {{ break-after: column; }}
        </style>
      </head>
      <body>
        <div class="multi">{lines}</div>
      </body>
    </html>
  "#,
    column_fill = column_fill,
    lines = lines
  )
}

#[test]
fn multicol_column_fill_balance_only_balances_last_page() {
  let html = multicol_balance_html("balance");
  let mut renderer = FastRender::new().unwrap();
  let dom = renderer.parse_html(&html).unwrap();
  let tree = renderer
    .layout_document_for_media(&dom, 400, 400, MediaType::Print)
    .unwrap();
  let page_roots = pages(&tree);

  assert!(
    page_roots.len() >= 3,
    "expected multicol content to span multiple pages, got {}",
    page_roots.len()
  );

  let (first_left, first_right) = count_text_fragments_by_column(page_roots[0], "x");
  assert!(
    first_left > first_right + 3,
    "balance should fill early fragments sequentially; got left={first_left} right={first_right}"
  );

  let last = *page_roots.last().expect("last page");
  let (last_left, last_right) = count_text_fragments_by_column(last, "x");
  assert!(last_left + last_right > 0);
  assert!(
    (last_left as isize - last_right as isize).abs() <= 1,
    "balance should balance the last fragment; got left={last_left} right={last_right}"
  );
}

#[test]
fn multicol_column_fill_balance_all_balances_every_page() {
  let html = multicol_balance_html("balance-all");
  let mut renderer = FastRender::new().unwrap();
  let dom = renderer.parse_html(&html).unwrap();
  let tree = renderer
    .layout_document_for_media(&dom, 400, 400, MediaType::Print)
    .unwrap();
  let page_roots = pages(&tree);

  assert!(
    page_roots.len() >= 3,
    "expected multicol content to span multiple pages, got {}",
    page_roots.len()
  );

  let (first_left, first_right) = count_text_fragments_by_column(page_roots[0], "x");
  assert!(
    (first_left as isize - first_right as isize).abs() <= 1,
    "balance-all should balance early fragments; got left={first_left} right={first_right}"
  );

  let last = *page_roots.last().expect("last page");
  let (last_left, last_right) = count_text_fragments_by_column(last, "x");
  assert!(last_left + last_right > 0);
  assert!(
    (last_left as isize - last_right as isize).abs() <= 1,
    "balance-all should balance the last fragment; got left={last_left} right={last_right}"
  );
}

#[test]
fn page_break_before_forces_new_page() {
  let html = r#"
    <html>
      <head>
        <style>
          @page { size: 200px 200px; margin: 0; }
          body { margin: 0; }
          .block { height: 80px; }
          #forced { page-break-before: always; }
        </style>
      </head>
      <body>
        <div class="block">Before</div>
        <div id="forced" class="block">Forced break</div>
        <div class="block">After</div>
      </body>
    </html>
  "#;

  let mut renderer = FastRender::new().unwrap();
  let dom = renderer.parse_html(html).unwrap();
  let tree = renderer
    .layout_document_for_media(&dom, 400, 400, MediaType::Print)
    .unwrap();
  let page_roots = pages(&tree);

  assert!(
    page_roots.len() >= 2,
    "expected pagination to create multiple pages"
  );

  let first = page_roots[0];
  let second = page_roots[1];

  assert!(
    find_text(first, "Before").is_some(),
    "first page should contain the preceding content"
  );
  assert!(
    find_text(first, "Forced break").is_none(),
    "forced break content must start a new page"
  );
  assert!(
    find_text(first, "After").is_none(),
    "content after the break should not remain on the first page"
  );

  assert!(
    find_text(second, "Forced break").is_some(),
    "forced break content should move to the next page"
  );
  assert!(
    find_text(second, "After").is_some(),
    "following content should flow after the forced page break"
  );
}

#[test]
fn page_break_before_column_keyword_is_ignored() {
  let html = r#"
    <html>
      <head>
        <style>
          @page { size: 100px 100px; margin: 0; }
          body { margin: 0; }
          #a { height: 10px; }
          /* `page-break-before` is a legacy alias with a restricted keyword set; `column` should be
             ignored rather than treated as a valid break value. */
          #b { height: 10px; background: rgb(7, 8, 9); page-break-before: column; }
        </style>
      </head>
      <body>
        <div id="a"></div>
        <div id="b"></div>
      </body>
    </html>
  "#;

  let mut renderer = FastRender::new().unwrap();
  let dom = renderer.parse_html(html).unwrap();
  let tree = renderer
    .layout_document_for_media(&dom, 200, 200, MediaType::Print)
    .unwrap();

  let page_roots = pages(&tree);
  let b_fragment = page_roots
    .iter()
    .find_map(|page| find_fragment_by_background(page, Rgba::rgb(7, 8, 9)))
    .expect("#b fragment");
  let style = b_fragment.style.as_ref().expect("#b computed style");
  assert_eq!(
    style.break_before,
    BreakBetween::Auto,
    "invalid legacy page-break-before values must be ignored"
  );
}

#[test]
fn page_break_inside_avoid_does_not_prevent_column_breaks() {
  let html = r#"
    <html>
      <head>
        <style>
          @page { size: 200px 200px; margin: 0; }
          body { margin: 0; }
          div { margin: 0; padding: 0; }
          #multicol { column-count: 2; column-gap: 0; width: 200px; height: 100px; }
          #a { height: 60px; }
          /* Legacy page-break-inside: avoid should map to break-inside: avoid-page, which must not
             suppress column breaks (only page breaks). */
          #b { page-break-inside: avoid; background: rgb(1, 2, 3); }
          #b1 { height: 40px; }
          #b2 { height: 20px; }
          #c { height: 10px; }
        </style>
      </head>
      <body>
        <div id="multicol">
          <div id="a">A</div>
          <div id="b">
            <div id="b1">B1</div>
            <div id="b2">B2</div>
          </div>
          <div id="c">C</div>
        </div>
      </body>
    </html>
  "#;

  let mut renderer = FastRender::new().unwrap();
  let dom = renderer.parse_html(html).unwrap();
  let tree = renderer
    .layout_document_for_media(&dom, 400, 400, MediaType::Print)
    .unwrap();
  let page_roots = pages(&tree);
  let page = page_roots[0];

  let b_fragment =
    find_fragment_by_background(page, Rgba::rgb(1, 2, 3)).expect("#b fragment with background");
  let style = b_fragment.style.as_ref().expect("#b computed style");
  assert_eq!(
    style.break_inside,
    BreakInside::AvoidPage,
    "page-break-inside: avoid must map to break-inside: avoid-page"
  );

  // Also sanity-check that the multicol container actually fragmented: at least one of the
  // descendants should land in the right half of the page content.
  let (_, b2_right) = count_text_fragments_by_column(page, "B2");
  assert_eq!(b2_right, 1);
}

#[test]
fn break_inside_avoid_page_keeps_flex_item_whole() {
  // Regression test: `break-inside: avoid-page` on flex items should prevent the item from being
  // split/clipped when it would fit on the next page.
  //
  // The flex item contains two 20px children, which creates a tempting break opportunity at y=90
  // (closer to the 100px page boundary than the item's start at y=70). The avoid-page item must be
  // moved entirely to the next page instead.
  let html = r#"
    <html>
      <head>
        <style>
          @page { size: 200px 100px; margin: 0; }
          html, body { margin: 0; padding: 0; }
          .flex { display: flex; flex-direction: column; width: 200px; }
          .spacer { height: 70px; flex: none; }
          .avoid {
            height: 40px;
            flex: none;
            break-inside: avoid-page;
            background: rgb(12, 34, 56);
          }
          .inner { height: 20px; }
        </style>
      </head>
      <body>
        <div class="flex">
          <div class="spacer"></div>
          <div class="avoid">
            <div class="inner"></div>
            <div class="inner"></div>
          </div>
        </div>
      </body>
    </html>
  "#;

  let mut renderer = FastRender::new().unwrap();
  let dom = renderer.parse_html(html).unwrap();
  let tree = renderer
    .layout_document_for_media(&dom, 400, 400, MediaType::Print)
    .unwrap();

  let page_roots = pages(&tree);
  assert!(
    page_roots.len() >= 2,
    "expected at least two pages, got {}",
    page_roots.len()
  );

  let color = Rgba::rgb(12, 34, 56);
  let pages_with_item: Vec<usize> = page_roots
    .iter()
    .enumerate()
    .filter_map(|(idx, page)| find_fragment_by_background(page, color).map(|_| idx))
    .collect();

  assert_eq!(
    pages_with_item,
    vec![1],
    "expected avoid-page flex item to appear only on page 2"
  );

  let item_fragment =
    find_fragment_by_background(page_roots[1], color).expect("avoid-page flex item fragment");
  assert!(
    (item_fragment.bounds.height() - 40.0).abs() < 0.1,
    "expected avoid-page flex item to keep full height on page 2 (got h={})",
    item_fragment.bounds.height()
  );
}

#[test]
fn webkit_page_break_inside_avoid_maps_to_avoid_page() {
  let html = r#"
    <html>
      <head>
        <style>
          @page { size: 100px 100px; margin: 0; }
          body { margin: 0; }
          #b { height: 10px; background: rgb(4, 5, 6); -webkit-page-break-inside: avoid; }
        </style>
      </head>
      <body>
        <div id="b"></div>
      </body>
    </html>
  "#;

  let mut renderer = FastRender::new().unwrap();
  let dom = renderer.parse_html(html).unwrap();
  let tree = renderer
    .layout_document_for_media(&dom, 200, 200, MediaType::Print)
    .unwrap();
  let page = pages(&tree)[0];

  let b_fragment =
    find_fragment_by_background(page, Rgba::rgb(4, 5, 6)).expect("#b fragment with background");
  let style = b_fragment.style.as_ref().expect("#b computed style");
  assert_eq!(
    style.break_inside,
    BreakInside::AvoidPage,
    "-webkit-page-break-inside: avoid must map to break-inside: avoid-page"
  );
}

#[test]
fn break_before_page_forces_new_page() {
  let html = r#"
    <html>
      <head>
        <style>
          @page { size: 100px 100px; margin: 0; }
          div { height: 40px; }
          .page-break { break-before: page; }
        </style>
      </head>
      <body>
        <div>A</div>
        <div class="page-break">B</div>
      </body>
    </html>
  "#;

  let mut renderer = FastRender::new().unwrap();
  let dom = renderer.parse_html(html).unwrap();
  let tree = renderer
    .layout_document_for_media(&dom, 200, 200, MediaType::Print)
    .unwrap();
  let page_roots = pages(&tree);

  assert!(find_text(page_roots[0], "A").is_some());
  assert!(find_text(page_roots[0], "B").is_none());
  assert!(page_roots
    .iter()
    .skip(1)
    .any(|page| find_text(page, "B").is_some()));
}

#[test]
fn break_before_column_does_not_force_page_without_columns() {
  let html = r#"
    <html>
      <head>
        <style>
          @page { size: 100px 100px; margin: 0; }
          div { height: 40px; }
          .column-break { break-before: column; }
        </style>
      </head>
      <body>
        <div>A</div>
        <div class="column-break">B</div>
      </body>
    </html>
  "#;

  let mut renderer = FastRender::new().unwrap();
  let dom = renderer.parse_html(html).unwrap();
  let tree = renderer
    .layout_document_for_media(&dom, 200, 200, MediaType::Print)
    .unwrap();
  let page_roots = pages(&tree);

  assert_eq!(page_roots.len(), 1);
  let page = page_roots[0];
  assert!(find_text(page, "A").is_some());
  assert!(find_text(page, "B").is_some());
}

#[test]
fn print_pagination_respects_widows_and_orphans() {
  let html = r#"
    <html>
      <head>
        <style>
          @page { size: 200px 30px; margin: 0; }
          body { margin: 0; }
          p { margin: 0; font-size: 10px; line-height: 10px; widows: 2; orphans: 2; }
        </style>
      </head>
      <body>
        <p>
          L1<br>
          L2<br>
          L3<br>
          L4<br>
          L5<br>
          L6<br>
          L7
        </p>
      </body>
    </html>
  "#;

  let mut renderer = FastRender::new().unwrap();
  let dom = renderer.parse_html(html).unwrap();
  let tree = renderer
    .layout_document_for_media(&dom, 200, 200, MediaType::Print)
    .unwrap();
  let page_roots = pages(&tree);

  assert_eq!(
    page_roots.len(),
    3,
    "expected 7 lines at 3 lines/page to paginate into 3 pages"
  );

  let lines = ["L1", "L2", "L3", "L4", "L5", "L6", "L7"];
  let mut seen = std::collections::HashSet::<&'static str>::new();

  for (idx, page) in page_roots.iter().enumerate() {
    let text = collected_text_compacted(page_content(page));
    let count = lines.iter().filter(|line| text.contains(**line)).count();
    assert!(
      count >= 2,
      "widows/orphans=2 should prevent 1-line pages when alternatives exist; page {} had {count} lines: {text}",
      idx + 1
    );
    for line in lines {
      if text.contains(line) {
        assert!(seen.insert(line), "line {line} duplicated across pages");
      }
    }
  }

  assert_eq!(
    seen.len(),
    lines.len(),
    "some lines were missing from output"
  );
}

#[test]
fn print_pagination_respects_break_before_avoid_page() {
  let html = r#"
    <html>
      <head>
        <style>
          @page { size: 200px 40px; margin: 0; }
          body { margin: 0; font-size: 8px; line-height: 8px; }
          div { height: 24px; }
          p { margin: 0; }
          #avoid { break-before: avoid-page; }
        </style>
      </head>
      <body>
        <div>Before</div>
        <p id="avoid">C1<br>C2<br>C3<br>C4<br>C5</p>
      </body>
    </html>
  "#;

  let mut renderer = FastRender::new().unwrap();
  let dom = renderer.parse_html(html).unwrap();
  let tree = renderer
    .layout_document_for_media(&dom, 200, 200, MediaType::Print)
    .unwrap();
  let page_roots = pages(&tree);

  assert!(
    page_roots.len() >= 2,
    "expected avoid-page test content to paginate"
  );
  let first_text = collected_text_compacted(page_content(page_roots[0]));
  assert!(
    first_text.contains("C1"),
    "break-before: avoid-page should prefer breaking inside the following block when possible; page1 text={first_text}"
  );
}

#[test]
fn print_pagination_honors_forced_breaks_even_when_content_fits() {
  let html = r#"
    <html>
      <head>
        <style>
          @page { size: 200px 200px; margin: 0; }
          body { margin: 0; }
          .container { break-inside: avoid; }
          .block { height: 40px; }
          #forced { break-before: page; }
        </style>
      </head>
      <body>
        <div class="container">
          <div class="block">Before</div>
          <div id="forced" class="block">Forced</div>
          <div class="block">After</div>
        </div>
      </body>
    </html>
  "#;

  let mut renderer = FastRender::new().unwrap();
  let dom = renderer.parse_html(html).unwrap();
  let tree = renderer
    .layout_document_for_media(&dom, 200, 200, MediaType::Print)
    .unwrap();
  let page_roots = pages(&tree);

  assert!(
    page_roots.len() >= 2,
    "expected forced break to create multiple pages even though content fits"
  );

  assert!(find_text(page_roots[0], "Before").is_some());
  assert!(find_text(page_roots[0], "Forced").is_none());
  assert!(find_text(page_roots[1], "Forced").is_some());
}

#[test]
fn print_pagination_respects_break_inside_avoid() {
  let html = r#"
    <html>
      <head>
        <style>
          @page { size: 200px 80px; margin: 0; }
          body { margin: 0; }
          .before { height: 40px; }
          .container { break-inside: avoid; }
          .item { height: 30px; }
        </style>
      </head>
      <body>
        <div class="before">Before</div>
        <div class="container">
          <div class="item">ITEM_A</div>
          <div class="item">ITEM_B</div>
        </div>
      </body>
    </html>
  "#;

  let mut renderer = FastRender::new().unwrap();
  let dom = renderer.parse_html(html).unwrap();
  let tree = renderer
    .layout_document_for_media(&dom, 200, 200, MediaType::Print)
    .unwrap();
  let page_roots = pages(&tree);

  assert_eq!(page_roots.len(), 2);

  assert!(find_text(page_roots[0], "Before").is_some());
  assert!(find_text(page_roots[0], "ITEM_A").is_none());
  assert!(find_text(page_roots[0], "ITEM_B").is_none());

  assert!(find_text(page_roots[1], "ITEM_A").is_some());
  assert!(find_text(page_roots[1], "ITEM_B").is_some());
}

#[test]
fn grid_item_with_break_inside_avoid_page_is_not_clipped_across_pages() {
  // Regression: grid items that avoid page breaks should be treated as atomic and moved to the
  // next page when they fit, rather than being clipped by the page boundary.
  let html = r#"
    <html>
      <head>
        <style>
          @page { size: 200px 100px; margin: 0; }
          body { margin: 0; }
          .grid {
            display: grid;
            height: 120px;
            grid-template-columns: 200px;
            /* The second track is intentionally larger than the page height so track-atomic
               fragmentation can't save us: break-inside on the grid item must be honored. */
            grid-template-rows: 80px 120px;
            align-items: start;
          }
          .spacer { grid-row: 1 / 2; height: 80px; }
          #target {
            grid-row: 2 / 3;
            height: 40px;
            break-inside: avoid-page;
            background: rgb(255, 0, 0);
          }
        </style>
      </head>
      <body>
        <div class="grid">
          <div class="spacer"></div>
          <div id="target"></div>
        </div>
      </body>
    </html>
  "#;

  let mut renderer = FastRender::new().unwrap();
  let dom = renderer.parse_html(html).unwrap();
  let tree = renderer
    .layout_document_for_media(&dom, 400, 400, MediaType::Print)
    .unwrap();
  let page_roots = pages(&tree);
  assert!(
    page_roots.len() >= 2,
    "expected the grid item to paginate to at least two pages"
  );

  let red = Rgba::rgb(255, 0, 0);
  let mut found = Vec::new();
  for (idx, page) in page_roots.iter().enumerate() {
    if let Some(fragment) = find_fragment_by_background(page, red) {
      found.push((idx, fragment));
    }
  }

  assert_eq!(
    found.len(),
    1,
    "expected exactly one red fragment across pages (found={:?})",
    found.iter().map(|(idx, f)| (*idx, f.bounds)).collect::<Vec<_>>()
  );
  assert_eq!(
    found[0].0, 1,
    "expected the red grid item to appear only on page 2"
  );
  assert!(
    (found[0].1.bounds.height() - 40.0).abs() < 0.5,
    "expected the grid item to retain its full height on the next page (got h={}, bounds={:?})",
    found[0].1.bounds.height(),
    found[0].1.bounds
  );
}

#[test]
fn margin_box_without_content_is_not_generated() {
  let html = r#"
    <html>
      <head>
        <style>
          @page {
            size: 200px 200px;
            margin: 10px;
            @top-center { background: rgb(255, 0, 0); }
          }
        </style>
      </head>
      <body></body>
    </html>
  "#;

  let mut renderer = FastRender::new().unwrap();
  let dom = renderer.parse_html(html).unwrap();
  let tree = renderer
    .layout_document_for_media(&dom, 400, 400, MediaType::Print)
    .unwrap();
  let red = Rgba::rgb(255, 0, 0);

  for page in pages(&tree) {
    assert!(
      find_fragment_with_background(page, red).is_none(),
      "margin boxes without content should not generate fragments"
    );
  }
}

#[test]
fn margin_box_with_empty_string_content_is_generated() {
  let html = r#"
    <html>
      <head>
        <style>
          @page {
            size: 200px 200px;
            margin: 10px;
            @top-center { background: rgb(255, 0, 0); content: ""; }
          }
        </style>
      </head>
      <body></body>
    </html>
  "#;

  let mut renderer = FastRender::new().unwrap();
  let dom = renderer.parse_html(html).unwrap();
  let tree = renderer
    .layout_document_for_media(&dom, 400, 400, MediaType::Print)
    .unwrap();
  let red = Rgba::rgb(255, 0, 0);

  let mut found = None;
  for page in pages(&tree) {
    if let Some(fragment) = find_fragment_with_background(page, red) {
      found = Some(fragment);
      break;
    }
  }

  let fragment = found.expect("margin box with empty content should generate a fragment");
  assert!(fragment.bounds.width() > 0.0);
  assert!(fragment.bounds.height() > 0.0);
}

#[test]
fn margin_box_default_text_align_center_for_top_center() {
  let html = r#"
    <html>
      <head>
        <style>
          @page {
            size: 200px 120px;
            margin: 10px;
            @top-center { content: "A"; }
          }
        </style>
      </head>
      <body></body>
    </html>
  "#;

  let mut renderer = FastRender::new().unwrap();
  let dom = renderer.parse_html(html).unwrap();
  let tree = renderer
    .layout_document_for_media(&dom, 300, 300, MediaType::Print)
    .unwrap();
  let page = pages(&tree)[0];

  let (margin_box, text) =
    find_text_with_parent(page, "A").expect("top-center margin box text fragment");
  let offset = text.bounds.x() - margin_box.bounds.x();

  assert!(
    offset > margin_box.bounds.width() * 0.2,
    "text should not hug the left edge: offset={}, box width={}",
    offset,
    margin_box.bounds.width()
  );
}

#[test]
fn margin_box_default_text_align_right_for_top_right() {
  let html = r#"
    <html>
      <head>
        <style>
          @page {
            size: 200px 120px;
            margin: 10px;
            @top-right { content: "A"; }
          }
        </style>
      </head>
      <body></body>
    </html>
  "#;

  let mut renderer = FastRender::new().unwrap();
  let dom = renderer.parse_html(html).unwrap();
  let tree = renderer
    .layout_document_for_media(&dom, 300, 300, MediaType::Print)
    .unwrap();
  let page = pages(&tree)[0];

  let (margin_box, text) =
    find_text_with_parent(page, "A").expect("top-right margin box text fragment");
  let left_offset = text.bounds.x() - margin_box.bounds.x();
  let right_offset = margin_box.bounds.max_x() - text.bounds.max_x();

  assert!(
    left_offset > right_offset,
    "text should be closer to the right edge (left_offset={}, right_offset={})",
    left_offset,
    right_offset
  );
  assert!(
    right_offset < margin_box.bounds.width() * 0.2,
    "text should sit near the right edge"
  );
}

#[test]
fn floats_defer_to_next_page_and_clear_following_text() {
  let html = r#"
    <html>
      <head>
        <style>
          @page { size: 200px 200px; margin: 0; }
          body { margin: 0; }
          .spacer { height: 150px; }
          .float { float: left; width: 80px; height: 80px; }
          p { clear: both; margin: 0; }
        </style>
      </head>
      <body>
        <div class="spacer"></div>
        <div class="float">Float box</div>
        <p>After float paragraph</p>
      </body>
    </html>
  "#;

  let mut renderer = FastRender::new().unwrap();
  let dom = renderer.parse_html(html).unwrap();
  let tree = renderer
    .layout_document_for_media(&dom, 200, 200, MediaType::Print)
    .unwrap();
  let page_roots = pages(&tree);

  assert!(
    page_roots.len() >= 2,
    "float deferral should create a second page for overflow"
  );

  fn collect_float_bottoms(node: &FragmentNode, origin: (f32, f32), out: &mut Vec<f32>) {
    let current = (origin.0 + node.bounds.x(), origin.1 + node.bounds.y());
    if node
      .style
      .as_ref()
      .is_some_and(|style| style.float.is_floating())
    {
      out.push(current.1 + node.bounds.height());
    }
    for child in node.children.iter() {
      collect_float_bottoms(child, current, out);
    }
  }

  let mut first_page_floats = Vec::new();
  collect_float_bottoms(page_roots[0], (0.0, 0.0), &mut first_page_floats);
  assert!(
    first_page_floats.is_empty(),
    "float should not be clipped across the first page"
  );

  let mut second_page_floats = Vec::new();
  collect_float_bottoms(page_roots[1], (0.0, 0.0), &mut second_page_floats);
  assert_eq!(second_page_floats.len(), 1, "float should move to page 2");

  let float_bottom = second_page_floats[0];

  let text_pos =
    find_text_position(page_roots[1], "After float", (0.0, 0.0)).expect("paragraph on page 2");
  assert!(
    text_pos.1 >= float_bottom - 0.1,
    "clearing text should appear below the float"
  );
  let page_bottom = page_roots[1].bounds.y() + page_roots[1].bounds.height();
  assert!(
    float_bottom <= page_bottom + 0.1,
    "float should fit entirely within the second page"
  );
}

#[test]
fn tall_float_fragments_across_pages_and_clears_following_text() {
  let html = r#"
    <html>
      <head>
        <style>
          @page { size: 200px 200px; margin: 0; }
          body { margin: 0; }
          .float {
            float: left;
            width: 200px;
            height: 350px;
            background: rgb(255, 0, 0);
          }
          p { clear: both; margin: 0; }
        </style>
      </head>
      <body>
        <div class="float"></div>
        <p>After float paragraph</p>
      </body>
    </html>
  "#;

  let mut renderer = FastRender::new().unwrap();
  let dom = renderer.parse_html(html).unwrap();
  let tree = renderer
    .layout_document_for_media(&dom, 200, 200, MediaType::Print)
    .unwrap();
  let page_roots = pages(&tree);

  assert!(
    page_roots.len() >= 2,
    "expected tall float to require at least two pages"
  );

  fn collect_float_bottoms(node: &FragmentNode, origin: (f32, f32), out: &mut Vec<f32>) {
    let current = (origin.0 + node.bounds.x(), origin.1 + node.bounds.y());
    if node
      .style
      .as_ref()
      .is_some_and(|style| style.float.is_floating())
    {
      out.push(current.1 + node.bounds.height());
    }
    for child in node.children.iter() {
      collect_float_bottoms(child, current, out);
    }
  }

  let mut first_page_floats = Vec::new();
  collect_float_bottoms(page_roots[0], (0.0, 0.0), &mut first_page_floats);
  assert!(
    !first_page_floats.is_empty(),
    "tall float should appear on the first page (as a clipped fragment)"
  );

  let mut second_page_floats = Vec::new();
  collect_float_bottoms(page_roots[1], (0.0, 0.0), &mut second_page_floats);
  assert!(
    !second_page_floats.is_empty(),
    "tall float should continue onto the second page"
  );

  let float_bottom = second_page_floats.into_iter().fold(0.0f32, f32::max);
  let text_pos = find_text_position(page_roots[1], "After float paragraph", (0.0, 0.0))
    .expect("paragraph should appear on the second page");
  assert!(
    text_pos.1 >= float_bottom - 0.1,
    "clearing paragraph should appear below the float fragment"
  );
}

#[test]
fn fixed_height_float_with_children_keeps_empty_continuation_fragments() {
  let html = r#"
    <html>
      <head>
        <style>
          @page { size: 200px 200px; margin: 0; }
          body { margin: 0; }
          .float {
            float: left;
            width: 200px;
            height: 350px;
            background: rgb(255, 0, 0);
          }
          .child { height: 50px; }
          p { clear: both; margin: 0; }
        </style>
      </head>
      <body>
        <div class="float"><div class="child">Child</div></div>
        <p>After float paragraph</p>
      </body>
    </html>
  "#;

  let mut renderer = FastRender::new().unwrap();
  let dom = renderer.parse_html(html).unwrap();
  let tree = renderer
    .layout_document_for_media(&dom, 200, 200, MediaType::Print)
    .unwrap();
  let page_roots = pages(&tree);

  assert!(
    page_roots.len() >= 2,
    "expected fixed-height float to require at least two pages"
  );

  assert!(
    find_text(page_roots[0], "Child").is_some(),
    "float child should be on the first page"
  );

  fn collect_float_bottoms(node: &FragmentNode, origin: (f32, f32), out: &mut Vec<f32>) {
    let current = (origin.0 + node.bounds.x(), origin.1 + node.bounds.y());
    if node
      .style
      .as_ref()
      .is_some_and(|style| style.float.is_floating())
    {
      out.push(current.1 + node.bounds.height());
    }
    for child in node.children.iter() {
      collect_float_bottoms(child, current, out);
    }
  }

  let mut second_page_floats = Vec::new();
  collect_float_bottoms(page_roots[1], (0.0, 0.0), &mut second_page_floats);
  assert!(
    !second_page_floats.is_empty(),
    "float continuation fragment should exist on the second page even when its children are only on the first page"
  );
  let float_bottom = second_page_floats.into_iter().fold(0.0f32, f32::max);

  let after_pos = find_text_position(page_roots[1], "After float paragraph", (0.0, 0.0))
    .expect("paragraph should appear on the second page");
  assert!(
    after_pos.1 >= float_bottom - 0.1,
    "clearing paragraph should appear below the float fragment"
  );
}

#[test]
fn forced_break_inside_tall_float_does_not_block_main_flow() {
  let html = r#"
    <html>
      <head>
        <style>
          @page { size: 200px 200px; margin: 0; }
          body { margin: 0; }
          .float {
            float: left;
            width: 200px;
            background: rgb(255, 0, 0);
          }
          .part1 { height: 220px; break-after: page; }
          .part2 { height: 80px; }
          p { clear: both; margin: 0; }
        </style>
      </head>
      <body>
        <div class="float">
          <div class="part1">Part1</div>
          <div class="part2">Part2</div>
        </div>
        <p>After float paragraph</p>
      </body>
    </html>
  "#;

  let mut renderer = FastRender::new().unwrap();
  let dom = renderer.parse_html(html).unwrap();
  let tree = renderer
    .layout_document_for_media(&dom, 200, 200, MediaType::Print)
    .unwrap();
  let page_roots = pages(&tree);

  assert!(
    page_roots.len() >= 3,
    "expected forced break inside float to create a continuation page"
  );

  fn collect_float_bottoms(node: &FragmentNode, origin: (f32, f32), out: &mut Vec<f32>) {
    let current = (origin.0 + node.bounds.x(), origin.1 + node.bounds.y());
    if node
      .style
      .as_ref()
      .is_some_and(|style| style.float.is_floating())
    {
      out.push(current.1 + node.bounds.height());
    }
    for child in node.children.iter() {
      collect_float_bottoms(child, current, out);
    }
  }

  let mut second_page_floats = Vec::new();
  collect_float_bottoms(page_roots[1], (0.0, 0.0), &mut second_page_floats);
  let float_bottom = second_page_floats.into_iter().fold(0.0f32, f32::max);

  let after_pos = find_text_position(page_roots[1], "After float paragraph", (0.0, 0.0))
    .expect("main-flow paragraph should remain on the second page");
  assert!(
    after_pos.1 >= float_bottom - 0.1,
    "main-flow content should clear the float fragment instead of being pushed to a later page"
  );

  assert!(
    find_text(page_roots[2], "Part2").is_some(),
    "float continuation content should appear on the following page"
  );
  assert!(
    find_text(page_roots[2], "After float paragraph").is_none(),
    "main-flow paragraph should not be delayed until the continuation page"
  );
}

#[test]
fn rtl_direction_flips_first_page_side() {
  let html = r#"
    <html>
      <head>
        <style>
          html { direction: rtl; }
          @page { size: 200px 200px; margin: 20px; }
          @page :left { @top-center { content: "LEFT"; } }
          @page :right { @top-center { content: "RIGHT"; } }
          body { margin: 0; }
          .tall { height: 600px; }
        </style>
      </head>
      <body>
        <div class="tall"></div>
      </body>
    </html>
  "#;

  let mut renderer = FastRender::new().unwrap();
  let dom = renderer.parse_html(html).unwrap();
  let tree = renderer
    .layout_document_for_media(&dom, 400, 400, MediaType::Print)
    .unwrap();
  let page_roots = pages(&tree);

  assert!(page_roots.len() >= 2);
  assert!(find_text(page_roots[0], "LEFT").is_some());
  assert!(find_text(page_roots[0], "RIGHT").is_none());
  assert!(find_text(page_roots[1], "RIGHT").is_some());
}

#[test]
fn vertical_rl_flips_first_page_side() {
  let html = r#"
    <html>
      <head>
        <style>
          html { writing-mode: vertical-rl; }
          @page { size: 160px 200px; margin: 20px; }
          @page :left { @top-center { content: "LEFT"; } }
          @page :right { @top-center { content: "RIGHT"; } }
          body { margin: 0; }
          .block { block-size: 100px; inline-size: 40px; }
        </style>
      </head>
      <body>
        <div class="block">A</div>
        <div class="block">B</div>
        <div class="block">C</div>
        <div class="block">D</div>
      </body>
    </html>
  "#;

  let mut renderer = FastRender::new().unwrap();
  let dom = renderer.parse_html(html).unwrap();
  let tree = renderer
    .layout_document_for_media(&dom, 400, 400, MediaType::Print)
    .unwrap();
  let page_roots = pages(&tree);

  assert!(page_roots.len() >= 2);
  assert!(find_text(page_roots[0], "LEFT").is_some());
  assert!(find_text(page_roots[0], "RIGHT").is_none());
  assert!(find_text(page_roots[1], "RIGHT").is_some());
}

#[test]
fn recto_break_depends_on_page_progression() {
  let html = r#"
    <html>
      <head>
        <style>
          html { direction: rtl; }
          @page { size: 200px 200px; margin: 20px; }
          @page :blank { @top-center { content: "Blank"; } }
          body { margin: 0; }
          .first { height: 80px; }
          .second { break-before: recto; height: 80px; }
        </style>
      </head>
      <body>
        <div class="first">First</div>
        <div class="second">Second</div>
      </body>
    </html>
  "#;

  let mut renderer = FastRender::new().unwrap();
  let dom = renderer.parse_html(html).unwrap();
  let tree = renderer
    .layout_document_for_media(&dom, 400, 400, MediaType::Print)
    .unwrap();
  let page_roots = pages(&tree);

  assert_eq!(page_roots.len(), 3);

  let blank_page = page_roots[1];
  assert!(find_text(blank_page, "Blank").is_some());
  assert!(find_text(blank_page, "First").is_none());
  assert!(find_text(blank_page, "Second").is_none());

  assert!(find_text(page_roots[2], "Second").is_some());
}

#[test]
fn forced_start_side_suppresses_leading_blank_pages() {
  let html = r#"
    <html>
      <head>
        <style>
          @page { size: 200px 200px; margin: 20px; }
          @page :left { @top-center { content: "LEFT"; } }
          @page :right { @top-center { content: "RIGHT"; } }
          body { margin: 0; }
          .first { break-before: left; height: 80px; }
        </style>
      </head>
      <body>
        <div class="first">Content</div>
      </body>
    </html>
  "#;

  let mut renderer = FastRender::new().unwrap();
  let dom = renderer.parse_html(html).unwrap();
  let tree = renderer
    .layout_document_for_media(&dom, 400, 400, MediaType::Print)
    .unwrap();
  let page_roots = pages(&tree);

  assert_eq!(page_roots.len(), 1);
  assert!(find_text(page_roots[0], "Content").is_some());
  assert!(find_text(page_roots[0], "LEFT").is_some());
  assert!(find_text(page_roots[0], "RIGHT").is_none());
}

#[test]
fn forced_start_side_suppresses_leading_blank_pages_with_padding() {
  let html = r#"
    <html>
      <head>
        <style>
          @page { size: 200px 200px; margin: 20px; }
          @page :left { @top-center { content: "LEFT"; } }
          @page :right { @top-center { content: "RIGHT"; } }
          body { margin: 0; padding-top: 20px; }
          .first { break-before: left; height: 80px; }
        </style>
      </head>
      <body>
        <div class="first">Content</div>
      </body>
    </html>
  "#;

  let mut renderer = FastRender::new().unwrap();
  let dom = renderer.parse_html(html).unwrap();
  let tree = renderer
    .layout_document_for_media(&dom, 400, 400, MediaType::Print)
    .unwrap();
  let page_roots = pages(&tree);

  assert_eq!(
    page_roots.len(),
    1,
    "forced break-before side at document start should not emit an extra page"
  );
  assert!(find_text(page_roots[0], "Content").is_some());
  assert!(find_text(page_roots[0], "LEFT").is_some());
  assert!(find_text(page_roots[0], "RIGHT").is_none());
}

#[test]
fn page_progression_uses_root_element_direction() {
  let html = r#"
    <html>
      <head>
        <style>
          html { direction: rtl; }
          body { direction: ltr; margin: 0; }
          @page { size: 200px 200px; margin: 20px; }
          @page :left { @top-center { content: "LEFT"; } }
          @page :right { @top-center { content: "RIGHT"; } }
          .tall { height: 600px; }
        </style>
      </head>
      <body>
        <div class="tall"></div>
      </body>
    </html>
  "#;

  let mut renderer = FastRender::new().unwrap();
  let dom = renderer.parse_html(html).unwrap();
  let tree = renderer
    .layout_document_for_media(&dom, 400, 400, MediaType::Print)
    .unwrap();
  let page_roots = pages(&tree);

  assert!(page_roots.len() >= 2);
  assert!(find_text(page_roots[0], "LEFT").is_some());
  assert!(find_text(page_roots[0], "RIGHT").is_none());
  assert!(find_text(page_roots[1], "RIGHT").is_some());
}

#[test]
fn blank_page_inserted_for_forced_side() {
  let html = r#"
    <html>
      <head>
        <style>
          @page {
            size: 200px 200px;
            margin: 20px;
          }
          @page :blank {
            @top-center { content: "Blank"; }
          }
          body { margin: 0; }
          .first { height: 150px; }
          .second { break-before: right; height: 120px; }
        </style>
      </head>
      <body>
        <div class="first">First</div>
        <div class="second">Second</div>
      </body>
    </html>
  "#;

  let mut renderer = FastRender::new().unwrap();
  let dom = renderer.parse_html(html).unwrap();
  let tree = renderer
    .layout_document_for_media(&dom, 400, 400, MediaType::Print)
    .unwrap();
  let page_roots = pages(&tree);

  assert_eq!(page_roots.len(), 3);

  let blank_page = page_roots[1];
  assert!(find_text(blank_page, "Blank").is_some());
  assert!(find_text(blank_page, "First").is_none());
  assert!(find_text(blank_page, "Second").is_none());

  assert!(find_text(page_roots[2], "Second").is_some());
  assert!(find_text(page_roots[0], "Blank").is_none());
  assert!(find_text(page_roots[2], "Blank").is_none());
}

#[test]
fn blank_page_inserted_for_break_after_right() {
  let html = r#"
    <html>
      <head>
        <style>
          @page {
            size: 200px 200px;
            margin: 20px;
          }
          @page :blank {
            @top-center { content: "Blank"; }
          }
          body { margin: 0; }
          .first { height: 150px; break-after: right; }
          .second { height: 120px; }
        </style>
      </head>
      <body>
        <div class="first">First</div>
        <div class="second">Second</div>
      </body>
    </html>
  "#;

  let mut renderer = FastRender::new().unwrap();
  let dom = renderer.parse_html(html).unwrap();
  let tree = renderer
    .layout_document_for_media(&dom, 400, 400, MediaType::Print)
    .unwrap();
  let page_roots = pages(&tree);

  // First page is `:right` in LTR page progression. `break-after: right` forces the next page to
  // also be right, so pagination must insert a blank left page in-between.
  assert_eq!(page_roots.len(), 3);

  let blank_page = page_roots[1];
  assert!(find_text(blank_page, "Blank").is_some());
  assert!(find_text(blank_page, "First").is_none());
  assert!(find_text(blank_page, "Second").is_none());

  assert!(find_text(page_roots[0], "First").is_some());
  assert!(find_text(page_roots[2], "Second").is_some());
  assert!(find_text(page_roots[0], "Blank").is_none());
  assert!(find_text(page_roots[2], "Blank").is_none());
}

#[test]
fn running_strings_and_elements_carry_to_inserted_blank_page() {
  let html = r#"
    <html>
      <head>
        <style>
          @page {
            size: 200px 200px;
            margin: 20px;
            @top-center { content: string(chapter) " - " element(header, start); }
          }
          body { margin: 0; }
          h1 { margin: 0; string-set: chapter content(); }
          h2 { position: running(header); margin: 0; }

          .first { height: 150px; break-after: right; }
          .second { height: 120px; }
        </style>
      </head>
      <body>
        <h2>Header A</h2>
        <h1>Chapter A</h1>
        <div class="first">First</div>
        <div class="second">Second</div>
      </body>
    </html>
  "#;

  let mut renderer = FastRender::new().unwrap();
  let dom = renderer.parse_html(html).unwrap();
  let tree = renderer
    .layout_document_for_media(&dom, 400, 400, MediaType::Print)
    .unwrap();
  let page_roots = pages(&tree);

  // First page is `:right` in LTR page progression. `break-after: right` forces the next page to
  // also be right, so pagination must insert a blank left page in-between.
  assert_eq!(page_roots.len(), 3, "expected a single inserted blank page");

  let blank_page = page_roots[1];
  let blank_content_text = collected_text_compacted(page_content(blank_page));
  assert!(
    blank_content_text.is_empty(),
    "expected inserted blank page to have no in-flow content, got {blank_content_text:?}"
  );
  assert!(
    margin_boxes_contain_text(blank_page, "Chapter A - Header A"),
    "expected inserted blank page to carry running string/element values into margin boxes"
  );
}

#[test]
fn blank_page_inserted_for_break_after_right_propagated_to_container_end() {
  let html = r#"
    <html>
      <head>
        <style>
          @page {
            size: 200px 200px;
            margin: 20px;
          }
          @page :blank {
            @top-center { content: "Blank"; }
          }
          body { margin: 0; }
          .container { padding-bottom: 50px; }
          .first { height: 100px; break-after: right; }
          .second { height: 120px; }
        </style>
      </head>
      <body>
        <div class="container">
          <div class="first">First</div>
        </div>
        <div class="second">Second</div>
      </body>
    </html>
  "#;

  let mut renderer = FastRender::new().unwrap();
  let dom = renderer.parse_html(html).unwrap();
  let tree = renderer
    .layout_document_for_media(&dom, 400, 400, MediaType::Print)
    .unwrap();
  let page_roots = pages(&tree);

  assert_eq!(page_roots.len(), 3);

  let blank_page = page_roots[1];
  assert!(find_text(blank_page, "Blank").is_some());
  assert!(find_text(blank_page, "First").is_none());
  assert!(find_text(blank_page, "Second").is_none());

  assert!(find_text(page_roots[0], "First").is_some());
  assert!(find_text(page_roots[2], "Second").is_some());
  assert!(find_text(page_roots[0], "Blank").is_none());
  assert!(find_text(page_roots[2], "Blank").is_none());
}

#[test]
fn blank_page_inserted_for_break_after_left() {
  let html = r#"
    <html>
      <head>
        <style>
          @page {
            size: 200px 200px;
            margin: 20px;
          }
          @page :blank {
            @top-center { content: "Blank"; }
          }
          body { margin: 0; }
          .first { height: 80px; }
          .middle { break-before: page; height: 120px; break-after: left; }
          .second { height: 120px; }
        </style>
      </head>
      <body>
        <div class="first">First</div>
        <div class="middle">Middle</div>
        <div class="second">Second</div>
      </body>
    </html>
  "#;

  let mut renderer = FastRender::new().unwrap();
  let dom = renderer.parse_html(html).unwrap();
  let tree = renderer
    .layout_document_for_media(&dom, 400, 400, MediaType::Print)
    .unwrap();
  let page_roots = pages(&tree);

  // Page 2 is `:left` in LTR page progression. `break-after: left` forces the next page to also be
  // left, so pagination must insert a blank right page in-between.
  assert_eq!(page_roots.len(), 4);

  let blank_page = page_roots[2];
  assert!(find_text(blank_page, "Blank").is_some());
  assert!(find_text(blank_page, "First").is_none());
  assert!(find_text(blank_page, "Middle").is_none());
  assert!(find_text(blank_page, "Second").is_none());

  assert!(find_text(page_roots[0], "First").is_some());
  assert!(find_text(page_roots[1], "Middle").is_some());
  assert!(find_text(page_roots[3], "Second").is_some());
  assert!(find_text(page_roots[0], "Blank").is_none());
  assert!(find_text(page_roots[1], "Blank").is_none());
  assert!(find_text(page_roots[3], "Blank").is_none());
}

#[test]
fn blank_page_inserted_for_break_after_right_in_rtl_page_progression() {
  let html = r#"
    <html>
      <head>
        <style>
          html { direction: rtl; }
          @page {
            size: 200px 200px;
            margin: 20px;
          }
          @page :blank {
            @top-center { content: "Blank"; }
          }
          body { margin: 0; }
          .first { height: 80px; }
          .middle { break-before: page; height: 120px; break-after: right; }
          .second { height: 120px; }
        </style>
      </head>
      <body>
        <div class="first">First</div>
        <div class="middle">Middle</div>
        <div class="second">Second</div>
      </body>
    </html>
  "#;

  let mut renderer = FastRender::new().unwrap();
  let dom = renderer.parse_html(html).unwrap();
  let tree = renderer
    .layout_document_for_media(&dom, 400, 400, MediaType::Print)
    .unwrap();
  let page_roots = pages(&tree);

  // In RTL page progression, page 1 is `:left` and page 2 is `:right`. `.middle` is forced onto
  // page 2 via `break-before: page`; `break-after: right` then forces the next page to also be
  // right, so pagination must insert a blank left page in-between.
  assert_eq!(page_roots.len(), 4);

  let blank_page = page_roots[2];
  assert!(find_text(blank_page, "Blank").is_some());
  assert!(find_text(blank_page, "First").is_none());
  assert!(find_text(blank_page, "Middle").is_none());
  assert!(find_text(blank_page, "Second").is_none());

  assert!(find_text(page_roots[0], "First").is_some());
  assert!(find_text(page_roots[1], "Middle").is_some());
  assert!(find_text(page_roots[3], "Second").is_some());
  assert!(find_text(page_roots[0], "Blank").is_none());
  assert!(find_text(page_roots[1], "Blank").is_none());
  assert!(find_text(page_roots[3], "Blank").is_none());
}

#[test]
fn blank_pseudo_outweighs_right_pseudo_in_specificity() {
  let html = r#"
    <html>
      <head>
        <style>
          html { direction: rtl; }
          @page { size: 200px 200px; margin: 20px; }
          @page :blank { @top-center { content: "BLANK_HDR"; } }
          @page :right { @top-center { content: "RIGHT_HDR"; } }
          body { margin: 0; }
          .first { height: 80px; }
          .second { break-before: left; height: 160px; }
          .third { height: 80px; }
        </style>
      </head>
      <body>
        <div class="first"></div>
        <div class="second"></div>
        <div class="third"></div>
      </body>
    </html>
  "#;

  let mut renderer = FastRender::new().unwrap();
  let dom = renderer.parse_html(html).unwrap();
  let tree = renderer
    .layout_document_for_media(&dom, 400, 400, MediaType::Print)
    .unwrap();
  let page_roots = pages(&tree);

  assert_eq!(page_roots.len(), 4);

  // Page 2 is blank and on the right side; `:blank` should beat `:right` even though `:right`
  // is declared later in the stylesheet.
  assert!(find_text(page_roots[1], "BLANK_HDR").is_some());
  assert!(find_text(page_roots[1], "RIGHT_HDR").is_none());

  // The non-blank right page should still use the `:right` rule.
  assert!(find_in_margin_boxes(page_roots[3], "RIGHT_HDR").is_some());
  assert!(find_in_margin_boxes(page_roots[3], "BLANK_HDR").is_none());
}

#[test]
fn page_selector_requires_all_pseudo_classes_to_match() {
  let html = r#"
    <html>
      <head>
        <style>
          html { direction: rtl; }
          @page { size: 200px 200px; margin: 20px; }
          @page :right { @top-center { content: "RIGHT_HDR"; } }
          @page :blank:right { @top-center { content: "BLANK_RIGHT_HDR"; } }
          body { margin: 0; }
          .first { height: 80px; }
          .second { break-before: left; height: 160px; }
          .third { height: 80px; }
        </style>
      </head>
      <body>
        <div class="first"></div>
        <div class="second"></div>
        <div class="third"></div>
      </body>
    </html>
  "#;

  let mut renderer = FastRender::new().unwrap();
  let dom = renderer.parse_html(html).unwrap();
  let tree = renderer
    .layout_document_for_media(&dom, 400, 400, MediaType::Print)
    .unwrap();
  let page_roots = pages(&tree);

  assert_eq!(page_roots.len(), 4);

  // Page 2 is blank and right; `:blank:right` should match.
  assert!(find_text_eq(page_roots[1], "BLANK_RIGHT_HDR").is_some());
  assert!(find_text_eq(page_roots[1], "RIGHT_HDR").is_none());

  // Page 4 is right but not blank; `:blank:right` must not match.
  assert!(find_text_eq(page_roots[3], "RIGHT_HDR").is_some());
  assert!(find_text_eq(page_roots[3], "BLANK_RIGHT_HDR").is_none());
}

#[test]
fn paginated_trees_compute_scroll_metadata() {
  let html = r#"
    <html>
      <head>
        <style>
          @page { size: 200px 150px; margin: 0; }
          html, body {
            margin: 0;
            height: 100%;
            scroll-snap-type: y mandatory;
          }
          section { height: 120px; scroll-snap-align: start; }
        </style>
      </head>
      <body>
        <section>First</section>
        <section>Second</section>
        <section>Third</section>
      </body>
    </html>
  "#;

  let mut renderer = FastRender::new().unwrap();
  let dom = renderer.parse_html(html).unwrap();
  let tree = renderer
    .layout_document_for_media(&dom, 200, 150, MediaType::Print)
    .unwrap();

  assert!(
    !tree.additional_fragments.is_empty(),
    "expected pagination to produce multiple pages"
  );

  let metadata = tree
    .scroll_metadata
    .as_ref()
    .expect("paginated trees should populate scroll metadata");
  assert!(
    metadata.containers.iter().any(|c| c.uses_viewport_scroll),
    "expected viewport scroll metadata"
  );
}

#[test]
fn var_in_string_set_is_used_in_running_header() {
  let html = r#"
    <html>
      <head>
        <style>
          :root { --title: "Var Title"; }
          h1 { string-set: header var(--title); }
          @page {
            size: 200px 200px;
            margin: 20px;
            @top-center { content: string(header); }
          }
        </style>
      </head>
      <body>
        <h1>Ignored</h1>
        <div style="height: 250px"></div>
      </body>
    </html>
  "#;

  let mut renderer = FastRender::new().unwrap();
  let dom = renderer.parse_html(html).unwrap();
  let tree = renderer
    .layout_document_for_media(&dom, 400, 400, MediaType::Print)
    .unwrap();
  let page_roots = pages(&tree);

  assert!(
    page_roots.len() >= 2,
    "expected at least two pages, got {}",
    page_roots.len()
  );

  for (idx, page) in page_roots.iter().take(2).enumerate() {
    let content_y = page_content_start_y(page);

    let mut texts = Vec::new();
    collect_text_fragments(page, (0.0, 0.0), &mut texts);

    texts.retain(|t| t.y < content_y);
    texts.sort_by(|a, b| {
      a.y
        .partial_cmp(&b.y)
        .unwrap_or(std::cmp::Ordering::Equal)
        .then(a.x.partial_cmp(&b.x).unwrap_or(std::cmp::Ordering::Equal))
    });

    let mut margin_text = String::new();
    for t in &texts {
      margin_text.push_str(&t.text);
    }
    let mut compacted = margin_text.clone();
    compacted.retain(|c| !c.is_whitespace());
    let mut expected = "Var Title".to_string();
    expected.retain(|c| !c.is_whitespace());

    assert!(
      compacted.contains(&expected),
      "expected page {} running header to include {expected:?}, got {margin_text:?}",
      idx + 1
    );
    assert!(
      !compacted.contains("Ignored"),
      "expected page {} running header to come from var() value, got {margin_text:?}",
      idx + 1
    );
  }
}

#[test]
fn var_in_string_argument_selects_last_running_string() {
  let html = r#"
    <html>
      <head>
        <style>
          h1 {
            string-set: header content();
            margin: 0;
            font-size: 20px;
            line-height: 20px;
          }
          :root { --pos: last; }
          @page {
            size: 200px 200px;
            margin: 20px;
            @top-center { content: string(header, var(--pos)); }
          }
        </style>
      </head>
      <body>
        <div style="height: 80px"></div>
        <div style="height: 80px"></div>
        <h1>First</h1>
        <div style="height: 60px"></div>
        <h1>Second</h1>
      </body>
    </html>
  "#;

  let mut renderer = FastRender::new().unwrap();
  let dom = renderer.parse_html(html).unwrap();
  let tree = renderer
    .layout_document_for_media(&dom, 400, 400, MediaType::Print)
    .unwrap();
  let page_roots = pages(&tree);

  assert!(
    page_roots.len() >= 2,
    "expected at least two pages, got {}",
    page_roots.len()
  );

  let page = page_roots[1];
  let content_y = page_content_start_y(page);

  let mut texts = Vec::new();
  collect_text_fragments(page, (0.0, 0.0), &mut texts);
  let header = texts
    .iter()
    .find(|t| t.text.contains("Second") && t.y < content_y)
    .expect("running header");

  assert_eq!(header.text, "Second");
}

#[test]
fn page_background_paints_page_box() {
  let html = r#"
    <html>
      <head>
        <style>
          @page {
            size: 200px 200px;
            margin: 0;
            background: rgb(10, 20, 30);
          }
          html, body { margin: 0; background: transparent; }
          body { padding: 40px; }
        </style>
      </head>
      <body>
        <div style="width: 20px; height: 20px; margin: 20px; background: white;"></div>
      </body>
    </html>
  "#;

  let mut renderer = FastRender::new().unwrap();
  let pixmap = renderer
    .render_html_with_options(
      html,
      RenderOptions::new()
        .with_viewport(200, 200)
        .with_media_type(MediaType::Print),
    )
    .expect("render page background");

  assert_eq!(pixel(&pixmap, 100, 100), [10, 20, 30, 255]);
}

#[test]
fn page_marks_crop_draws_lines_in_bleed_area() {
  let html = r#"
    <html>
      <head>
        <style>
          @page {
            size: 100px 100px;
            margin: 0;
            bleed: 20px;
            trim: 0;
            marks: crop;
            color: black;
            background: white;
          }
          html, body { margin: 0; background: transparent; }
        </style>
      </head>
      <body></body>
    </html>
  "#;

  let mut renderer = FastRender::new().unwrap();
  let pixmap = renderer
    .render_html_with_options(
      html,
      RenderOptions::new()
        // Total page size includes bleed on both sides: 100 + 2*20.
        .with_viewport(140, 140)
        .with_media_type(MediaType::Print),
    )
    .expect("render page crop marks");

  let bg = [255, 255, 255, 255];
  let mark = [0, 0, 0, 255];

  // Sample both the horizontal and vertical crop mark segments near each corner.
  let samples: &[(u32, u32)] = &[
    // Top-left.
    (15, 19),
    (19, 15),
    // Top-right.
    (125, 19),
    (120, 15),
    // Bottom-left.
    (15, 120),
    (19, 125),
    // Bottom-right.
    (125, 120),
    (120, 125),
  ];
  for &(x, y) in samples {
    assert_eq!(
      pixel(&pixmap, x, y),
      mark,
      "expected crop mark pixel at ({x},{y}) to be non-background"
    );
  }

  // Ensure marks don't leak into the main page area.
  assert_eq!(pixel(&pixmap, 70, 70), bg);
}

#[test]
fn page_marks_cross_draws_lines_in_bleed_area() {
  let html = r#"
    <html>
      <head>
        <style>
          @page {
            size: 100px 100px;
            margin: 0;
            bleed: 20px;
            trim: 0;
            marks: cross;
            color: black;
            background: white;
          }
          html, body { margin: 0; background: transparent; }
        </style>
      </head>
      <body></body>
    </html>
  "#;

  let mut renderer = FastRender::new().unwrap();
  let pixmap = renderer
    .render_html_with_options(
      html,
      RenderOptions::new()
        .with_viewport(140, 140)
        .with_media_type(MediaType::Print),
    )
    .expect("render page cross marks");

  let bg = [255, 255, 255, 255];
  let mark = [0, 0, 0, 255];

  // Cross marks are drawn near each corner; sample the intersection point for each.
  let samples: &[(u32, u32)] = &[(15, 15), (125, 15), (15, 125), (125, 125)];
  for &(x, y) in samples {
    assert_eq!(
      pixel(&pixmap, x, y),
      mark,
      "expected cross mark pixel at ({x},{y}) to be non-background"
    );
  }

  assert_eq!(pixel(&pixmap, 70, 70), bg);
}

#[test]
fn page_marks_none_draws_no_marks() {
  let html = r#"
    <html>
      <head>
        <style>
          @page {
            size: 100px 100px;
            margin: 0;
            bleed: 20px;
            trim: 0;
            marks: none;
            color: black;
            background: white;
          }
          html, body { margin: 0; background: transparent; }
        </style>
      </head>
      <body></body>
    </html>
  "#;

  let mut renderer = FastRender::new().unwrap();
  let pixmap = renderer
    .render_html_with_options(
      html,
      RenderOptions::new()
        .with_viewport(140, 140)
        .with_media_type(MediaType::Print),
    )
    .expect("render page marks none");

  let bg = [255, 255, 255, 255];
  let samples: &[(u32, u32)] = &[
    (15, 19),
    (19, 15),
    (125, 19),
    (120, 15),
    (15, 120),
    (19, 125),
    (125, 120),
    (120, 125),
  ];
  for &(x, y) in samples {
    assert_eq!(
      pixel(&pixmap, x, y),
      bg,
      "expected marks:none pixel at ({x},{y}) to match background"
    );
  }
}

#[test]
fn page_bleed_expands_total_page_size_and_insets_content_origin() {
  let html = r#"
    <html>
      <head>
        <style>
          @page {
            size: 200px 200px;
            margin: 0;
            bleed: 10px;
            background: rgb(10, 20, 30);
          }
          html, body { margin: 0; background: transparent; }
        </style>
      </head>
      <body>
        <div style="height: 1px"></div>
      </body>
    </html>
  "#;

  // Layout assertions: bleed expands the page root bounds and insets the document wrapper.
  let mut renderer = FastRender::new().unwrap();
  let dom = renderer.parse_html(html).unwrap();
  let tree = renderer
    .layout_document_for_media(&dom, 400, 400, MediaType::Print)
    .unwrap();
  let page = pages(&tree)[0];

  assert!((page.bounds.width() - 220.0).abs() < 0.1);
  assert!((page.bounds.height() - 220.0).abs() < 0.1);
  let wrapper = page_document_wrapper(page);
  assert!(
    (wrapper.bounds.x() - 10.0).abs() < 0.1,
    "expected wrapper x=bleed (10px), got {}",
    wrapper.bounds.x()
  );
  assert!(
    (wrapper.bounds.y() - 10.0).abs() < 0.1,
    "expected wrapper y=bleed (10px), got {}",
    wrapper.bounds.y()
  );

  // Rendering assertion: bleed area should paint the page background.
  let pixmap = renderer
    .render_html_with_options(
      html,
      RenderOptions::new()
        .with_viewport(220, 220)
        .with_media_type(MediaType::Print),
    )
    .expect("render paged media with bleed");
  assert_eq!(
    pixel(&pixmap, 0, 0),
    [10, 20, 30, 255],
    "expected page background to paint into the bleed area"
  );
}

#[test]
fn page_trim_reduces_page_box_and_insets_wrapper() {
  let html = r#"
    <html>
      <head>
        <style>
          @page {
            size: 200px 200px;
            margin: 0;
            bleed: 10px;
            trim: 5px;
            background: rgb(180, 190, 200);
            border: 2px solid rgb(20, 40, 60);
          }
          html, body { margin: 0; background: transparent; }
        </style>
      </head>
      <body>
        <div style="height: 1px"></div>
      </body>
    </html>
  "#;

  let mut renderer = FastRender::new().unwrap();
  let dom = renderer.parse_html(html).unwrap();
  let tree = renderer
    .layout_document_for_media(&dom, 400, 400, MediaType::Print)
    .unwrap();
  let page = pages(&tree)[0];

  assert!((page.bounds.width() - 220.0).abs() < 0.1);
  assert!((page.bounds.height() - 220.0).abs() < 0.1);

  let wrapper = page_document_wrapper(page);
  assert!(
    (wrapper.bounds.x() - 15.0).abs() < 0.1,
    "expected wrapper x=bleed+trim (15px), got {}",
    wrapper.bounds.x()
  );
  assert!(
    (wrapper.bounds.y() - 15.0).abs() < 0.1,
    "expected wrapper y=bleed+trim (15px), got {}",
    wrapper.bounds.y()
  );
  assert!(
    (wrapper.bounds.width() - 190.0).abs() < 0.1,
    "expected wrapper width=size-2*trim (190px), got {}",
    wrapper.bounds.width()
  );
  assert!(
    (wrapper.bounds.height() - 190.0).abs() < 0.1,
    "expected wrapper height=size-2*trim (190px), got {}",
    wrapper.bounds.height()
  );

  let pixmap = renderer
    .render_html_with_options(
      html,
      RenderOptions::new()
        .with_viewport(220, 220)
        .with_media_type(MediaType::Print),
    )
    .expect("render paged media with bleed+trim");

  // (5,5) is in the bleed area. It should show only the page background, not the page border.
  assert_eq!(
    pixel(&pixmap, 5, 5),
    [180, 190, 200, 255],
    "expected border to be inset from bleed edge"
  );
  // (16,16) is inside the page border (bleed+trim=15, border=2).
  assert_eq!(pixel(&pixmap, 16, 16), [20, 40, 60, 255]);
}

#[test]
fn document_canvas_background_paints_above_page_background() {
  let html = r#"
    <html>
      <head>
        <style>
          @page {
            size: 200px 200px;
            margin: 0;
            background: rgb(255, 0, 0);
          }
          html { margin: 0; background: transparent; }
          body {
            margin: 0;
            height: 50px;
            background: rgb(0, 0, 255);
          }
        </style>
      </head>
      <body></body>
    </html>
  "#;

  let mut renderer = FastRender::new().unwrap();
  let pixmap = renderer
    .render_html_with_options(
      html,
      RenderOptions::new()
        .with_viewport(200, 200)
        .with_media_type(MediaType::Print),
    )
    .expect("render paged media canvas background");

  assert_eq!(pixel(&pixmap, 100, 150), [0, 0, 255, 255]);
}

#[test]
fn document_canvas_background_does_not_paint_into_page_margins() {
  let html = r#"
    <html>
      <head>
        <style>
          @page {
            size: 200px 200px;
            margin: 40px;
            background: rgb(255, 0, 0);
          }
          html { margin: 0; background: transparent; }
          body {
            margin: 0;
            height: 10px;
            background: rgb(0, 0, 255);
          }
        </style>
      </head>
      <body></body>
    </html>
  "#;

  let mut renderer = FastRender::new().unwrap();
  let pixmap = renderer
    .render_html_with_options(
      html,
      RenderOptions::new()
        .with_viewport(200, 200)
        .with_media_type(MediaType::Print),
    )
    .expect("render paged media canvas background with margins");

  assert_eq!(pixel(&pixmap, 10, 10), [255, 0, 0, 255]);
  assert_eq!(pixel(&pixmap, 100, 100), [0, 0, 255, 255]);
}

#[test]
fn page_border_is_painted() {
  let html = r#"
     <html>
       <head>
        <style>
          @page {
            size: 200px 200px;
            margin: 0;
            background: rgb(180, 190, 200);
            border: 12px solid rgb(20, 40, 60);
          }
          html, body { margin: 0; background: transparent; }
          body { padding: 30px; }
        </style>
      </head>
      <body>
        <div style="width: 10px; height: 10px; margin: 20px; background: white;"></div>
      </body>
    </html>
  "#;

  let mut renderer = FastRender::new().unwrap();
  let pixmap = renderer
    .render_html_with_options(
      html,
      RenderOptions::new()
        .with_viewport(200, 200)
        .with_media_type(MediaType::Print),
    )
    .expect("render page border");

  assert_eq!(pixel(&pixmap, 10, 5), [20, 40, 60, 255]);
  assert_eq!(pixel(&pixmap, 100, 100), [180, 190, 200, 255]);
}

#[test]
fn page_border_insets_document_contents() {
  let html = r#"
    <html>
      <head>
        <style>
          @page {
            size: 200px 200px;
            margin: 0;
            background: rgb(180, 190, 200);
            border: 12px solid rgb(20, 40, 60);
          }
          html, body { margin: 0; background: transparent; }
          #cover {
            position: absolute;
            inset: 0;
            background: rgb(0, 0, 255);
          }
        </style>
      </head>
      <body>
        <div id="cover"></div>
      </body>
    </html>
  "#;

  let mut renderer = FastRender::new().unwrap();
  let pixmap = renderer
    .render_html_with_options(
      html,
      RenderOptions::new()
        .with_viewport(200, 200)
        .with_media_type(MediaType::Print),
    )
    .expect("render page border + absolute content");

  assert_eq!(
    pixel(&pixmap, 5, 5),
    [20, 40, 60, 255],
    "document contents should not overlap the page border"
  );
  assert_eq!(pixel(&pixmap, 100, 100), [0, 0, 255, 255]);
}

#[test]
fn document_canvas_background_paints_under_page_padding() {
  let html = r#"
    <html>
      <head>
        <style>
          @page {
            size: 200px 200px;
            margin: 0;
            background: rgb(180, 190, 200);
            border: 10px solid rgb(20, 40, 60);
            padding: 20px;
          }
          html { margin: 0; background: transparent; }
          body { margin: 0; background: rgb(0, 0, 255); }
        </style>
      </head>
      <body></body>
    </html>
  "#;

  let mut renderer = FastRender::new().unwrap();
  let pixmap = renderer
    .render_html_with_options(
      html,
      RenderOptions::new()
        .with_viewport(200, 200)
        .with_media_type(MediaType::Print),
    )
    .expect("render page padding with document canvas background");

  assert_eq!(pixel(&pixmap, 5, 5), [20, 40, 60, 255]);
  assert_eq!(
    pixel(&pixmap, 15, 15),
    [0, 0, 255, 255],
    "page padding area should be filled by the document canvas background"
  );
  assert_eq!(pixel(&pixmap, 100, 100), [0, 0, 255, 255]);
}

#[test]
fn box_decoration_break_clone_paints_borders_on_each_page_fragment() {
  let html = r#"
    <html>
      <head>
        <style>
          @page { size: 100px 100px; margin: 0; background: rgb(255, 255, 255); }
          html, body { margin: 0; background: rgb(255, 255, 255); }
          #box {
            height: 150px;
            background: rgb(255, 255, 255);
            border-top: 4px solid rgb(0, 0, 0);
            border-bottom: 4px solid rgb(0, 0, 0);
            box-decoration-break: clone;
          }
        </style>
      </head>
      <body>
        <div id="box"></div>
      </body>
    </html>
  "#;

  let mut renderer = FastRender::new().unwrap();
  let pixmap = renderer
    .render_html_with_options(
      html,
      RenderOptions::new()
        .with_viewport(100, 220)
        .with_media_type(MediaType::Print),
    )
    .expect("render box-decoration-break: clone across pages");

  // Page 1 ends at y=99, page 2 begins at y=100 (PageStacking::Stacked { gap: 0 }).
  assert_eq!(
    pixel(&pixmap, 50, 98),
    [0, 0, 0, 255],
    "clone should paint the bottom border on the first fragment"
  );
  assert_eq!(
    pixel(&pixmap, 50, 101),
    [0, 0, 0, 255],
    "clone should paint the top border on the continuation fragment"
  );
}

#[test]
fn box_decoration_break_slice_omits_internal_fragment_edges() {
  let html = r#"
    <html>
      <head>
        <style>
          @page { size: 100px 100px; margin: 0; background: rgb(255, 255, 255); }
          html, body { margin: 0; background: rgb(255, 255, 255); }
          #box {
            height: 150px;
            background: rgb(255, 255, 255);
            border-top: 4px solid rgb(0, 0, 0);
            border-bottom: 4px solid rgb(0, 0, 0);
            box-decoration-break: slice;
          }
        </style>
      </head>
      <body>
        <div id="box"></div>
      </body>
    </html>
  "#;

  let mut renderer = FastRender::new().unwrap();
  let pixmap = renderer
    .render_html_with_options(
      html,
      RenderOptions::new()
        .with_viewport(100, 220)
        .with_media_type(MediaType::Print),
    )
    .expect("render box-decoration-break: slice across pages");

  assert_eq!(
    pixel(&pixmap, 50, 1),
    [0, 0, 0, 255],
    "slice should paint the top border on the first fragment"
  );

  // Slice should only paint at the outer edges of the fragmented box, skipping internal edges.
  assert_eq!(
    pixel(&pixmap, 50, 98),
    [255, 255, 255, 255],
    "slice should omit the bottom border at the internal fragment edge"
  );
  assert_eq!(
    pixel(&pixmap, 50, 101),
    [255, 255, 255, 255],
    "slice should omit the top border at the internal fragment edge"
  );

  let last_black_y = (0..pixmap.height())
    .rev()
    .find(|&y| pixel(&pixmap, 50, y) == [0, 0, 0, 255])
    .expect("expected slice to paint a bottom border somewhere on the last fragment");
  assert!(
    last_black_y > 100,
    "expected bottom border to appear on page 2, got last black pixel at y={last_black_y}"
  );
}

#[test]
fn margin_boxes_paint_above_fixed_high_z_content_by_default() {
  let html = r#"
      <html>
       <head>
        <style>
          @page {
            size: 200px 200px;
            margin: 50px;
            @top-left-corner {
              content: "";
              background: rgb(255, 0, 0);
            }
          }
          html, body { margin: 0; background: transparent; }
          #fixed {
            position: fixed;
            /* Move into the page margin area so it overlaps @top-left-corner. */
            top: -30px;
            left: -30px;
            width: 200px;
            height: 200px;
            background: rgb(0, 0, 255);
            z-index: 9999;
          }
        </style>
      </head>
      <body>
        <div id="fixed"></div>
      </body>
    </html>
  "#;

  let mut renderer = FastRender::new().unwrap();
  let pixmap = renderer
    .render_html_with_options(
      html,
      RenderOptions::new()
        .with_viewport(200, 200)
        .with_media_type(MediaType::Print),
    )
    .expect("render paged media");

  assert_eq!(pixel(&pixmap, 30, 30), [255, 0, 0, 255]);
}

#[test]
fn negative_z_margin_boxes_paint_behind_document_contents() {
  let html = r#"
    <html>
      <head>
        <style>
          @page {
            size: 200px 200px;
            margin: 50px;
            @top-left-corner {
              content: "";
              background: rgb(255, 0, 0);
              z-index: -1;
            }
          }
          html, body { margin: 0; background: transparent; }
          #fixed {
            position: fixed;
            /* Move into the page margin area so it overlaps @top-left-corner. */
            top: -30px;
            left: -30px;
            width: 200px;
            height: 200px;
            background: rgb(0, 0, 255);
            z-index: 9999;
          }
        </style>
      </head>
      <body>
        <div id="fixed"></div>
      </body>
    </html>
  "#;

  let mut renderer = FastRender::new().unwrap();
  let pixmap = renderer
    .render_html_with_options(
      html,
      RenderOptions::new()
        .with_viewport(200, 200)
        .with_media_type(MediaType::Print),
    )
    .expect("render paged media");

  assert_eq!(pixel(&pixmap, 30, 30), [0, 0, 255, 255]);
}

#[test]
fn margin_box_z_index_orders_overlapping_boxes() {
  let html = r#"
    <html>
      <head>
        <style>
          @page {
            size: 200px 200px;
            margin: 50px;
            @top-left-corner {
              content: "";
              background: rgb(255, 0, 0);
              z-index: 2;
            }
            @bottom-right-corner {
              content: "";
              background: rgb(0, 255, 0);
              /* Translate into the top-left-corner so the two margin boxes overlap. */
              transform: translate(-150px, -150px);
              z-index: 1;
            }
          }
          html, body { margin: 0; background: transparent; }
        </style>
      </head>
      <body></body>
    </html>
  "#;

  let mut renderer = FastRender::new().unwrap();
  let pixmap = renderer
    .render_html_with_options(
      html,
      RenderOptions::new()
        .with_viewport(200, 200)
        .with_media_type(MediaType::Print),
    )
    .expect("render paged media");

  assert_eq!(pixel(&pixmap, 30, 30), [255, 0, 0, 255]);
}

#[test]
fn vertical_writing_mode_paginate_along_block_axis() {
  let html = r#"
    <html>
      <head>
        <style>
          @page { size: 120px 200px; margin: 0; }
          html { writing-mode: vertical-rl; }
          body { margin: 0; }
          .block { block-size: 100px; inline-size: 40px; }
        </style>
      </head>
      <body>
        <div class="block">Before</div>
        <div class="block">After</div>
      </body>
    </html>
  "#;

  let mut renderer = FastRender::new().unwrap();
  let dom = renderer.parse_html(html).unwrap();
  let tree = renderer
    .layout_document_for_media(&dom, 300, 300, MediaType::Print)
    .unwrap();
  let page_roots = pages(&tree);

  assert!(
    page_roots.len() >= 2,
    "expected pagination along the horizontal block axis to create multiple pages"
  );

  let first_content = page_content(page_roots[0]);
  assert!(find_text(first_content, "Before").is_some());
  assert!(
    find_text(first_content, "After").is_none(),
    "later content should not appear on the first page"
  );

  let second_content = page_content(page_roots[1]);
  assert!(find_text(second_content, "After").is_some());
}

#[test]
fn vertical_writing_forced_break_respected() {
  let html = r#"
    <html>
      <head>
        <style>
          @page { size: 120px 200px; margin: 0; }
          html { writing-mode: vertical-rl; }
          body { margin: 0; }
          .block { block-size: 40px; inline-size: 40px; }
          #forced { break-before: page; }
        </style>
      </head>
      <body>
        <div class="block">Start</div>
        <div id="forced" class="block">Forced</div>
        <div class="block">Tail</div>
      </body>
    </html>
  "#;

  let mut renderer = FastRender::new().unwrap();
  let dom = renderer.parse_html(html).unwrap();
  let tree = renderer
    .layout_document_for_media(&dom, 300, 300, MediaType::Print)
    .unwrap();
  let page_roots = pages(&tree);

  assert!(
    page_roots.len() >= 2,
    "expected forced break to create a new page in vertical writing mode"
  );

  let first_content = page_content(page_roots[0]);
  assert!(find_text(first_content, "Start").is_some());
  assert!(find_text(first_content, "Forced").is_none());

  let second_content = page_content(page_roots[1]);
  assert!(find_text(second_content, "Forced").is_some());
}

#[test]
fn footnote_policy_auto_defers_body_without_moving_call() {
  let html = r#"
    <html>
      <head>
        <style>
          @page { size: 200px 40px; margin: 0; }
          body { margin: 0; font-size: 10px; line-height: 10px; }
          .line { height: 20px; }
          .note { float: footnote; footnote-policy: auto; display: inline-block; height: 20px; }
        </style>
      </head>
      <body>
        <div class="line">Alpha</div>
        <div class="line">Beta <span class="note">Footnote body</span></div>
      </body>
    </html>
  "#;

  let mut renderer = FastRender::new().unwrap();
  let dom = renderer.parse_html(html).unwrap();
  let tree = renderer
    .layout_document_for_media(&dom, 200, 40, MediaType::Print)
    .unwrap();

  let page_roots = pages(&tree);
  assert_eq!(page_roots.len(), 2, "expected a deferred footnote to create a second page");

  let page1 = page_roots[0];
  let wrapper1 = page_document_wrapper(page1);
  assert_eq!(wrapper1.children.len(), 1, "page 1 should have no footnote area");
  assert!(find_text(page_content(page1), "1").is_some(), "call marker should stay on page 1");

  let page2 = page_roots[1];
  let wrapper2 = page_document_wrapper(page2);
  assert_eq!(wrapper2.children.len(), 2, "page 2 should have a footnote area");
  let footnote_area2 = wrapper2.children.get(1).expect("page 2 footnote area");
  assert!(
    find_text(footnote_area2, "Footnote body").is_some(),
    "deferred footnote body should be placed on page 2"
  );
  assert!(
    find_text(page_content(page2), "1").is_none(),
    "call marker must not be moved to page 2"
  );
}

#[test]
fn footnote_policy_block_forces_break_before_paragraph() {
  let html = r#"
    <html>
      <head>
        <style>
          @page { size: 200px 80px; margin: 0; }
          body { margin: 0; font-size: 10px; line-height: 9px; }
          p { margin: 0; widows: 1; orphans: 1; }
          .header { height: 35px; }
          .note { float: footnote; footnote-policy: block; display: inline-block; height: 30px; }
        </style>
      </head>
      <body>
        <div class="header">Header</div>
        <p>L1<br>L2<br>L3 <span class="note">Footnote body</span></p>
      </body>
    </html>
  "#;

  let mut renderer = FastRender::new().unwrap();
  let dom = renderer.parse_html(html).unwrap();
  let tree = renderer
    .layout_document_for_media(&dom, 200, 80, MediaType::Print)
    .unwrap();
  let page_roots = pages(&tree);

  assert!(
    page_roots.len() >= 2,
    "expected at least two pages for footnote-policy:block test"
  );

  let page1 = page_roots[0];
  let content1 = page_content(page1);
  assert!(find_text(content1, "Header").is_some());
  assert!(
    find_text(content1, "L1").is_none(),
    "paragraph containing the footnote should be moved entirely to the next page"
  );

  let page2 = page_roots[1];
  let wrapper2 = page_document_wrapper(page2);
  let content2 = page_content(page2);
  assert!(find_text(content2, "L1").is_some());
  assert!(find_text(content2, "L2").is_some());
  assert!(find_text(content2, "L3").is_some());
  assert_eq!(
    wrapper2.children.len(),
    2,
    "page 2 should have a footnote area"
  );
  let footnote_area2 = wrapper2.children.get(1).expect("page 2 footnote area");
  assert!(find_text(footnote_area2, "Footnote body").is_some());
}

#[test]
fn footnote_policy_line_forces_break_at_reference_line() {
  let html = r#"
    <html>
      <head>
        <style>
          @page { size: 200px 80px; margin: 0; }
          body { margin: 0; font-size: 10px; line-height: 9px; }
          p { margin: 0; widows: 1; orphans: 1; }
          .header { height: 35px; }
          .note { float: footnote; footnote-policy: line; display: inline-block; height: 30px; }
        </style>
      </head>
      <body>
        <div class="header">Header</div>
        <p>L1<br>L2<br>L3 <span class="note">Footnote body</span></p>
      </body>
    </html>
  "#;

  let mut renderer = FastRender::new().unwrap();
  let dom = renderer.parse_html(html).unwrap();
  let tree = renderer
    .layout_document_for_media(&dom, 200, 80, MediaType::Print)
    .unwrap();
  let page_roots = pages(&tree);

  assert!(
    page_roots.len() >= 2,
    "expected at least two pages for footnote-policy:line test"
  );

  let page1 = page_roots[0];
  let content1 = page_content(page1);
  assert!(find_text(content1, "Header").is_some());
  assert!(find_text(content1, "L1").is_some());
  assert!(find_text(content1, "L2").is_some());
  assert!(
    find_text(content1, "L3").is_none(),
    "line containing the footnote should be moved to the next page"
  );

  let page2 = page_roots[1];
  let wrapper2 = page_document_wrapper(page2);
  let content2 = page_content(page2);
  assert!(find_text(content2, "L3").is_some());
  assert_eq!(
    wrapper2.children.len(),
    2,
    "page 2 should have a footnote area"
  );
  let footnote_area2 = wrapper2.children.get(1).expect("page 2 footnote area");
  assert!(find_text(footnote_area2, "Footnote body").is_some());
}

#[test]
fn footnote_float_does_not_detach_without_pagination() {
  let html = r#"
    <html>
      <head>
        <style>
          body { margin: 0; font-size: 10px; line-height: 10px; }
          p { margin: 0; }
          .note { float: footnote; }
        </style>
      </head>
      <body>
        <p>Main<span class="note">Footnote body</span></p>
      </body>
    </html>
  "#;

  let mut renderer = FastRender::new().unwrap();
  let dom = renderer.parse_html(html).unwrap();
  let tree = renderer.layout_document(&dom, 200, 100).unwrap();

  assert!(find_text(&tree.root, "Main").is_some());
  assert!(
    find_text(&tree.root, "Footnote body").is_some(),
    "without pagination, float: footnote should not remove the footnote body from the flow"
  );

  fn contains_footnote_anchor(node: &FragmentNode) -> bool {
    if matches!(node.content, FragmentContent::FootnoteAnchor { .. }) {
      return true;
    }
    node.children.iter().any(contains_footnote_anchor)
  }

  let mut has_anchor = contains_footnote_anchor(&tree.root);
  for fragment in tree.additional_fragments.iter() {
    has_anchor |= contains_footnote_anchor(fragment);
  }
  assert!(
    !has_anchor,
    "expected `float: footnote` to be treated as a normal element when pagination is disabled"
  );
}

#[test]
fn footnote_counter_increment_can_override_implicit_footnote_numbering() {
  let html = r#"
    <html>
      <head>
        <style>
          @page { size: 200px 100px; margin: 0; }
          body { margin: 0; font-size: 10px; line-height: 10px; }
          p { margin: 0; }
          .note { float: footnote; counter-increment: footnote 0; }
        </style>
      </head>
      <body>
        <p>Main<span class="note">Footnote body</span></p>
      </body>
    </html>
  "#;

  let mut renderer = FastRender::new().unwrap();
  let dom = renderer.parse_html(html).unwrap();
  let tree = renderer
    .layout_document_for_media(&dom, 200, 100, MediaType::Print)
    .unwrap();

  let page_roots = pages(&tree);
  assert!(!page_roots.is_empty());
  let page1 = page_roots[0];
  let wrapper = page_document_wrapper(page1);
  assert_eq!(wrapper.children.len(), 2);

  let content = page_content(page1);
  let footnote_area = wrapper.children.get(1).expect("footnote area");

  assert!(find_text(content, "Main").is_some());
  assert!(
    find_text(content, "Footnote body").is_none(),
    "footnote body should be removed from main flow"
  );
  assert!(
    find_text(content, "0").is_some(),
    "authored `counter-increment: footnote 0` should override implicit footnote numbering at the call site"
  );
  assert!(find_text(content, "1").is_none());

  assert!(find_text(footnote_area, "Footnote body").is_some());
  assert!(
    find_text(footnote_area, "0").is_some(),
    "authored `counter-increment: footnote 0` should be reflected in the footnote marker"
  );
  assert!(find_text(footnote_area, "1").is_none());
}

#[test]
fn footnote_counter_set_can_override_implicit_footnote_numbering() {
  let html = r#"
    <html>
      <head>
        <style>
          @page { size: 200px 100px; margin: 0; }
          body { margin: 0; font-size: 10px; line-height: 10px; }
          p { margin: 0; }
          .note { float: footnote; counter-set: footnote 5; }
        </style>
      </head>
      <body>
        <p>Main<span class="note">Footnote body</span></p>
      </body>
    </html>
  "#;

  let mut renderer = FastRender::new().unwrap();
  let dom = renderer.parse_html(html).unwrap();
  let tree = renderer
    .layout_document_for_media(&dom, 200, 100, MediaType::Print)
    .unwrap();

  let page_roots = pages(&tree);
  assert!(!page_roots.is_empty());
  let page1 = page_roots[0];
  let wrapper = page_document_wrapper(page1);
  assert_eq!(wrapper.children.len(), 2);

  let content = page_content(page1);
  let footnote_area = wrapper.children.get(1).expect("footnote area");

  assert!(find_text(content, "Main").is_some());
  assert!(
    find_text(content, "Footnote body").is_none(),
    "footnote body should be removed from main flow"
  );
  assert!(
    find_text(content, "5").is_some(),
    "authored `counter-set: footnote 5` should override implicit footnote numbering at the call site"
  );
  assert!(find_text(content, "1").is_none());

  assert!(find_text(footnote_area, "Footnote body").is_some());
  assert!(
    find_text(footnote_area, "5").is_some(),
    "authored `counter-set: footnote 5` should be reflected in the footnote marker"
  );
  assert!(find_text(footnote_area, "1").is_none());
}

#[test]
fn footnote_body_images_resolve_intrinsic_sizes() {
  let html = r#"
    <html>
      <head>
        <style>
          @page { size: 200px 100px; margin: 0; }
          body { margin: 0; font-size: 10px; line-height: 10px; }
          p { margin: 0; }
          img { display: block; }
          .note { float: footnote; }
        </style>
      </head>
      <body>
        <p>Main<span class="note"><img src="data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMB/6XWcJAAAAAASUVORK5CYII="></span></p>
      </body>
    </html>
  "#;

  let mut renderer = FastRender::new().unwrap();
  let dom = renderer.parse_html(html).unwrap();
  let tree = renderer
    .layout_document_for_media(&dom, 200, 100, MediaType::Print)
    .unwrap();

  let page_roots = pages(&tree);
  assert_eq!(
    page_roots.len(),
    1,
    "expected the single page to accommodate a 1x1 intrinsic image footnote"
  );
  let page1 = page_roots[0];
  let wrapper = page_document_wrapper(page1);
  assert_eq!(
    wrapper.children.len(),
    2,
    "page with footnote should have content + footnote area"
  );

  let footnote_area = wrapper.children.get(1).expect("footnote area");
  let image = find_replaced_image(footnote_area).expect("expected image fragment in footnote area");
  assert!(
    (image.bounds.width() - 1.0).abs() < 0.1,
    "expected intrinsic 1px image width (got {})",
    image.bounds.width()
  );
  assert!(
    (image.bounds.height() - 1.0).abs() < 0.1,
    "expected intrinsic 1px image height (got {})",
    image.bounds.height()
  );
}

#[test]
fn footnote_float_generates_call_and_page_footnote_area() {
  let html = r#"
     <html>
       <head>
         <style>
          @page { size: 200px 100px; margin: 0; }
          body { margin: 0; font-size: 10px; line-height: 10px; }
          p { margin: 0; }
          .note { float: footnote; }
          .page2 { break-before: page; }
        </style>
      </head>
      <body>
        <p>Main<span class="note">Footnote body</span></p>
        <p class="page2">Page2</p>
      </body>
    </html>
  "#;

  let mut renderer = FastRender::new().unwrap();
  let dom = renderer.parse_html(html).unwrap();
  let tree = renderer
    .layout_document_for_media(&dom, 200, 100, MediaType::Print)
    .unwrap();
  let page_roots = pages(&tree);
  assert!(page_roots.len() >= 2);

  let page1 = page_roots[0];
  let wrapper1 = page_document_wrapper(page1);
  assert_eq!(
    wrapper1.children.len(),
    2,
    "page with footnote should have content + footnote area"
  );
  let content = page_content(page1);
  let footnote_area = wrapper1.children.get(1).expect("footnote area");

  assert!(find_text(content, "Main").is_some());
  assert!(
    find_text(content, "Footnote body").is_none(),
    "footnote body should be removed from main flow"
  );
  assert!(
    find_text(content, "1").is_some(),
    "footnote call marker should be inserted at call site"
  );

  assert!(find_text(footnote_area, "Footnote body").is_some());
  assert!(
    find_text(footnote_area, "1").is_some(),
    "footnote marker should be present in footnote area"
  );

  let page2 = page_roots[1];
  let wrapper2 = page_document_wrapper(page2);
  assert_eq!(
    wrapper2.children.len(),
    1,
    "pages without footnotes should not include a footnote area"
  );
  assert!(find_text(page2, "Page2").is_some());
}

#[test]
fn footnote_float_as_flex_item_generates_page_footnote_area() {
  let html = r#"
    <html>
      <head>
        <style>
          @page { size: 200px 100px; margin: 0; }
          body { margin: 0; font-size: 10px; line-height: 10px; }
          .flex { display: flex; }
          .note { float: footnote; }
        </style>
      </head>
      <body>
        <div class="flex">X<span class="note">Footnote body</span>Y</div>
      </body>
    </html>
  "#;

  let mut renderer = FastRender::new().unwrap();
  let dom = renderer.parse_html(html).unwrap();
  let tree = renderer
    .layout_document_for_media(&dom, 200, 100, MediaType::Print)
    .unwrap();
  let page_roots = pages(&tree);
  assert!(!page_roots.is_empty());

  let page1 = page_roots[0];
  let wrapper = page_document_wrapper(page1);
  assert_eq!(
    wrapper.children.len(),
    2,
    "page with footnote should have content + footnote area"
  );
  let content = page_content(page1);
  let footnote_area = wrapper.children.get(1).expect("footnote area");

  assert!(find_text(content, "X").is_some());
  assert!(find_text(content, "Y").is_some());
  assert!(find_text(content, "Footnote body").is_none());
  assert!(find_text(footnote_area, "Footnote body").is_some());
}

#[test]
fn footnote_float_as_grid_item_generates_page_footnote_area() {
  let html = r#"
    <html>
      <head>
        <style>
          @page { size: 200px 100px; margin: 0; }
          body { margin: 0; font-size: 10px; line-height: 10px; }
          .grid { display: grid; grid-template-columns: 1fr; }
          .note { float: footnote; }
        </style>
      </head>
      <body>
        <div class="grid">X<span class="note">Footnote body</span>Y</div>
      </body>
    </html>
  "#;

  let mut renderer = FastRender::new().unwrap();
  let dom = renderer.parse_html(html).unwrap();
  let tree = renderer
    .layout_document_for_media(&dom, 200, 100, MediaType::Print)
    .unwrap();
  let page_roots = pages(&tree);
  assert!(!page_roots.is_empty());

  let page1 = page_roots[0];
  let wrapper = page_document_wrapper(page1);
  assert_eq!(
    wrapper.children.len(),
    2,
    "page with footnote should have content + footnote area"
  );
  let content = page_content(page1);
  let footnote_area = wrapper.children.get(1).expect("footnote area");

  assert!(find_text(content, "X").is_some());
  assert!(find_text(content, "Y").is_some());
  assert!(find_text(content, "Footnote body").is_none());
  assert!(find_text(footnote_area, "Footnote body").is_some());
}

#[test]
fn footnote_area_in_vertical_writing_mode_is_positioned_at_block_end() {
  let html = r#"
    <html>
      <head>
        <style>
          @page { size: 200px 100px; margin: 0; }
          html { writing-mode: vertical-rl; }
          body { margin: 0; font-size: 10px; line-height: 10px; }
          p { margin: 0; }
          .note { float: footnote; }
        </style>
      </head>
      <body>
        <p>Main<span class="note">Footnote body</span></p>
      </body>
    </html>
  "#;

  let mut renderer = FastRender::new().unwrap();
  let dom = renderer.parse_html(html).unwrap();
  let tree = renderer
    .layout_document_for_media(&dom, 200, 100, MediaType::Print)
    .unwrap();

  let page_roots = pages(&tree);
  assert!(!page_roots.is_empty());
  let page1 = page_roots[0];
  let wrapper = page_document_wrapper(page1);
  assert_eq!(
    wrapper.children.len(),
    2,
    "page with footnote should have content + footnote area"
  );

  let content = page_content(page1);
  let footnote_area = wrapper.children.get(1).expect("footnote area");

  assert!(find_text(content, "Main").is_some());
  assert!(
    find_text(content, "Footnote body").is_none(),
    "footnote body should be removed from main flow"
  );
  assert!(find_text(footnote_area, "Footnote body").is_some());

  // In `writing-mode: vertical-rl`, the page block axis is horizontal and points in the negative
  // direction, so block-end is at the physical left edge.
  let epsilon = 0.1;
  assert!(
    (footnote_area.bounds.x() - content.bounds.x()).abs() < epsilon,
    "expected footnote area to align with the page content box block-end (physical left) in vertical-rl (content.x={}, footnote_area.x={})",
    content.bounds.x(),
    footnote_area.bounds.x()
  );

  // The footnote separator may be represented as a synthetic 1px child fragment, or as a border on
  // the footnote area itself. Either way, it must be aligned to the block-start edge (physical
  // right edge) in `vertical-rl` and span the page inline axis.
  if footnote_area.children.len() >= 2 {
    let separator = footnote_area.children.first().expect("footnote separator");
    assert!(
      (separator.bounds.width() - 1.0).abs() < epsilon,
      "expected separator thickness of 1px along the horizontal block axis (got {})",
      separator.bounds.width()
    );
    assert!(
      (separator.bounds.height() - footnote_area.bounds.height()).abs() < epsilon,
      "expected separator to span the full inline axis (got {}, expected {})",
      separator.bounds.height(),
      footnote_area.bounds.height()
    );
    assert!(
      (separator.bounds.max_x() - footnote_area.bounds.width()).abs() < epsilon,
      "expected separator to align with the block-start edge (physical right) in vertical-rl (separator.max_x={}, footnote_area.width={})",
      separator.bounds.max_x(),
      footnote_area.bounds.width()
    );
  } else if let Some(style) = footnote_area.style.as_deref() {
    assert_eq!(
      style.border_right_width.unit,
      LengthUnit::Px,
      "expected border-right width to resolve to px (got {:?})",
      style.border_right_width
    );
    assert_eq!(
      style.border_left_width.unit,
      LengthUnit::Px,
      "expected border-left width to resolve to px (got {:?})",
      style.border_left_width
    );
    assert!(
      (style.border_right_width.value - 1.0).abs() < epsilon
        && style.border_left_width.value.abs() < epsilon,
      "expected separator border on the physical right edge in vertical-rl (right={:?}, left={:?})",
      style.border_right_width,
      style.border_left_width
    );
  } else {
    panic!("expected footnote separator to be represented as a child fragment or a border");
  }
}

#[test]
fn footnote_body_in_multicol_uses_page_width_in_footnote_area() {
  let html = r#"
    <html>
      <head>
        <style>
          @page { size: 200px 200px; margin: 0; }
          body { margin: 0; font-size: 10px; line-height: 10px; }
          p { margin: 0; }
          .multi { column-count: 2; column-gap: 0; }
          .note { float: footnote; display: block; }
        </style>
      </head>
      <body>
        <div class="multi">
          <p>
            Alpha<span class="note">
              Footnote body with enough words to wrap across lines when constrained to a single
              column.
            </span>
          </p>
        </div>
      </body>
    </html>
  "#;

  let mut renderer = FastRender::new().unwrap();
  let dom = renderer.parse_html(html).unwrap();
  let tree = renderer
    .layout_document_for_media(&dom, 200, 200, MediaType::Print)
    .unwrap();
  let page_roots = pages(&tree);
  assert!(!page_roots.is_empty());

  let page1 = page_roots[0];
  let wrapper = page_document_wrapper(page1);
  assert_eq!(
    wrapper.children.len(),
    2,
    "page with footnote should have content + footnote area"
  );
  let page_width = page_content(page1).bounds.width();

  let footnote_area = wrapper.children.get(1).expect("footnote area");
  assert!(
    footnote_area.children.len() >= 2,
    "expected footnote area to include separator + footnote body"
  );
  let footnote_body = footnote_area.children.get(1).expect("footnote body");

  // The footnote body should be laid out using the page footnote area width (full page content
  // box), not the call site's column width.
  assert!(
    (footnote_body.bounds.width() - page_width).abs() < 0.1,
    "expected footnote body width to match page content width (page_width={page_width}, footnote_width={})",
    footnote_body.bounds.width()
  );
}

#[test]
fn footnote_area_orders_multicol_footnotes_in_reading_order() {
  let mut lines = String::new();
  for idx in 0..18 {
    lines.push_str(&format!(r#"<div class="line">Line {idx}</div>"#));
  }
  lines.push_str(r#"<div class="line">A<span class="note">FootnoteA</span></div>"#);
  lines.push_str(r#"<div class="line">Fill</div>"#);
  lines.push_str(r#"<div class="line">B<span class="note">FootnoteB</span></div>"#);

  let html = format!(
    r#"
    <html>
      <head>
        <style>
          @page {{ size: 200px 400px; margin: 0; }}
          body {{ margin: 0; font-size: 10px; line-height: 10px; }}
          .multi {{ column-count: 2; column-gap: 0; column-fill: auto; height: 200px; }}
          .line {{ height: 10px; }}
          .note {{ float: footnote; }}
        </style>
      </head>
      <body>
        <div class="multi">{lines}</div>
      </body>
    </html>
  "#
  );

  let mut renderer = FastRender::new().unwrap();
  let dom = renderer.parse_html(&html).unwrap();
  let tree = renderer
    .layout_document_for_media(&dom, 200, 400, MediaType::Print)
    .unwrap();
  let page_roots = pages(&tree);
  assert!(!page_roots.is_empty());

  let page1 = page_roots[0];
  let wrapper = page_document_wrapper(page1);
  assert_eq!(wrapper.children.len(), 2);
  let footnote_area = wrapper.children.get(1).expect("footnote area");

  let a_y = find_text_position(footnote_area, "FootnoteA", (0.0, 0.0))
    .expect("FootnoteA in footnote area")
    .1;
  let b_y = find_text_position(footnote_area, "FootnoteB", (0.0, 0.0))
    .expect("FootnoteB in footnote area")
    .1;
  assert!(
    a_y < b_y,
    "expected FootnoteA to appear before FootnoteB in the page footnote area (a_y={a_y}, b_y={b_y})"
  );
}

#[test]
fn footnote_body_snapshots_use_footnote_area_content_width() {
  let html = r#"
    <html>
      <head>
        <style>
          @page {
            size: 200px 200px;
            margin: 0;
            @footnote { padding-left: 50px; padding-right: 50px; }
          }
          body { margin: 0; font-size: 10px; line-height: 10px; }
          p { margin: 0; }
          .note { float: footnote; display: block; }
        </style>
      </head>
      <body>
        <p>
          Alpha<span class="note">
            Footnote body with enough words to wrap across lines when constrained by the footnote
            area's padding.
          </span>
        </p>
      </body>
    </html>
  "#;

  let mut renderer = FastRender::new().unwrap();
  let dom = renderer.parse_html(html).unwrap();
  let tree = renderer
    .layout_document_for_media(&dom, 200, 200, MediaType::Print)
    .unwrap();
  let page_roots = pages(&tree);
  assert!(!page_roots.is_empty());

  let page1 = page_roots[0];
  let wrapper = page_document_wrapper(page1);
  assert_eq!(
    wrapper.children.len(),
    2,
    "page with footnote should have content + footnote area"
  );

  let page_width = page_content(page1).bounds.width();

  let footnote_area = wrapper.children.get(1).expect("footnote area");
  assert!(
    footnote_area.children.len() >= 2,
    "expected footnote area to include separator + footnote body"
  );
  let footnote_body = footnote_area.children.get(1).expect("footnote body");

  let expected = (page_width - 100.0).max(0.0);
  assert!(
    (footnote_body.bounds.width() - expected).abs() < 0.1,
    "expected footnote body width to match footnote content width (expected={expected}, actual={})",
    footnote_body.bounds.width()
  );
}

#[test]
fn footnote_display_block_stacks_footnotes() {
  let html = r#"
    <html>
      <head>
        <style>
          @page { size: 200px 200px; margin: 0; }
          body { margin: 0; font-size: 10px; line-height: 10px; }
          p { margin: 0; }
          .note { float: footnote; display: inline-block; height: 10px; footnote-display: block; }
        </style>
      </head>
      <body>
        <p>Main<span class="note">First</span><span class="note">Second</span></p>
      </body>
    </html>
  "#;

  let mut renderer = FastRender::new().unwrap();
  let dom = renderer.parse_html(html).unwrap();
  let tree = renderer
    .layout_document_for_media(&dom, 200, 200, MediaType::Print)
    .unwrap();

  let page_roots = pages(&tree);
  assert!(!page_roots.is_empty());
  let page1 = page_roots[0];
  let wrapper = page_document_wrapper(page1);
  assert_eq!(wrapper.children.len(), 2);
  let footnote_area = wrapper.children.get(1).expect("footnote area");

  let (_, y_first) =
    find_text_position(footnote_area, "First", (0.0, 0.0)).expect("First in footnote area");
  let (_, y_second) =
    find_text_position(footnote_area, "Second", (0.0, 0.0)).expect("Second in footnote area");

  assert!(
    y_first < y_second - 0.5,
    "expected block footnotes to stack vertically (y_first={y_first}, y_second={y_second})"
  );
}

#[test]
fn footnote_display_inline_packs_footnotes_on_one_line() {
  let html = r#"
    <html>
      <head>
        <style>
          @page { size: 200px 200px; margin: 0; }
          body { margin: 0; font-size: 10px; line-height: 10px; }
          p { margin: 0; }
          .note { float: footnote; display: inline-block; height: 10px; footnote-display: inline; }
        </style>
      </head>
      <body>
        <p>Main<span class="note">First</span><span class="note">Second</span></p>
      </body>
    </html>
  "#;

  let mut renderer = FastRender::new().unwrap();
  let dom = renderer.parse_html(html).unwrap();
  let tree = renderer
    .layout_document_for_media(&dom, 200, 200, MediaType::Print)
    .unwrap();

  let page_roots = pages(&tree);
  assert!(!page_roots.is_empty());
  let page1 = page_roots[0];
  let wrapper = page_document_wrapper(page1);
  assert_eq!(wrapper.children.len(), 2);
  let footnote_area = wrapper.children.get(1).expect("footnote area");

  let (x_first, y_first) =
    find_text_position(footnote_area, "First", (0.0, 0.0)).expect("First in footnote area");
  let (x_second, y_second) =
    find_text_position(footnote_area, "Second", (0.0, 0.0)).expect("Second in footnote area");

  assert!(
    (y_first - y_second).abs() < 0.5,
    "expected inline footnotes to share a line (y_first={y_first}, y_second={y_second})"
  );
  assert!(
    x_second > x_first + 1.0,
    "expected inline footnotes to flow left-to-right (x_first={x_first}, x_second={x_second})"
  );
}

#[test]
fn footnote_display_compact_is_more_compact_than_block() {
  fn layout_footnote_area_height(footnote_display: &str) -> f32 {
    let html = format!(
      r#"
      <html>
        <head>
          <style>
            @page {{ size: 200px 200px; margin: 0; }}
            body {{ margin: 0; font-size: 10px; line-height: 10px; }}
            p {{ margin: 0; }}
            .note {{ float: footnote; display: inline-block; height: 10px; footnote-display: {footnote_display}; }}
          </style>
        </head>
        <body>
          <p>Main<span class="note">First</span><span class="note">Second</span></p>
        </body>
      </html>
    "#
    );

    let mut renderer = FastRender::new().unwrap();
    let dom = renderer.parse_html(&html).unwrap();
    let tree = renderer
      .layout_document_for_media(&dom, 200, 200, MediaType::Print)
      .unwrap();

    let page_roots = pages(&tree);
    assert!(!page_roots.is_empty());
    let page1 = page_roots[0];
    let wrapper = page_document_wrapper(page1);
    assert_eq!(wrapper.children.len(), 2);
    let footnote_area = wrapper.children.get(1).expect("footnote area");
    footnote_area.bounds.height()
  }

  let block_height = layout_footnote_area_height("block");
  let compact_height = layout_footnote_area_height("compact");

  assert!(
    compact_height < block_height - 0.5,
    "expected compact footnote layout to use <= the block footnote area height (block={block_height}, compact={compact_height})"
  );
}

#[test]
fn footnote_area_background_and_padding_paint() {
  let html = r#"
    <html>
      <head>
        <style>
          @page {
            size: 100px 100px;
            margin: 0;
            background: rgb(255, 255, 255);
            @footnote {
              background: rgb(0, 255, 0);
              padding-top: 10px;
              border-top: 2px solid rgb(255, 0, 0);
            }
          }
          html, body { margin: 0; background: transparent; }
          body { font-size: 10px; line-height: 10px; }
          p { margin: 0; }
          .note { float: footnote; display: block; height: 10px; }
        </style>
      </head>
      <body>
        <p>Main<span class="note">Footnote body</span></p>
      </body>
    </html>
  "#;

  let mut renderer = FastRender::new().unwrap();
  let pixmap = renderer
    .render_html_with_options(
      html,
      RenderOptions::new()
        .with_viewport(100, 100)
        .with_media_type(MediaType::Print),
    )
    .expect("render footnote area paint");

  // Above the footnote area should remain the page background.
  assert_eq!(pixel(&pixmap, 50, 70), [255, 255, 255, 255]);
  // The footnote area's border-top should be painted.
  assert_eq!(pixel(&pixmap, 50, 78), [255, 0, 0, 255]);
  // The padding area should be filled by the @footnote background.
  assert_eq!(pixel(&pixmap, 50, 83), [0, 255, 0, 255]);
}

#[test]
fn footnote_area_max_height_defers_excess_footnotes() {
  let html = r#"
    <html>
      <head>
        <style>
          @page { size: 200px 200px; margin: 0; @footnote { max-height: 20px; } }
          body { margin: 0; font-size: 10px; line-height: 10px; }
          p { margin: 0; }
          .note { float: footnote; display: block; height: 10px; }
        </style>
      </head>
      <body>
        <p>Alpha<span class="note">Footnote one</span></p>
        <p>Beta<span class="note">Footnote two</span></p>
      </body>
    </html>
  "#;

  let mut renderer = FastRender::new().unwrap();
  let dom = renderer.parse_html(html).unwrap();
  let tree = renderer
    .layout_document_for_media(&dom, 200, 200, MediaType::Print)
    .unwrap();
  let page_roots = pages(&tree);

  assert_eq!(
    page_roots.len(),
    2,
    "expected max-height to defer the second footnote to a new page"
  );

  let page1 = page_roots[0];
  let wrapper1 = page_document_wrapper(page1);
  assert_eq!(wrapper1.children.len(), 2);
  let content1 = page_content(page1);
  let footnote_area1 = wrapper1.children.get(1).expect("page 1 footnote area");
  assert!(find_text(content1, "Alpha").is_some());
  assert!(find_text(content1, "Beta").is_none());
  assert!(find_text(footnote_area1, "Footnote one").is_some());
  assert!(find_text(footnote_area1, "Footnote two").is_none());

  let page2 = page_roots[1];
  let wrapper2 = page_document_wrapper(page2);
  assert_eq!(wrapper2.children.len(), 2);
  let content2 = page_content(page2);
  let footnote_area2 = wrapper2.children.get(1).expect("page 2 footnote area");
  assert!(find_text(content2, "Beta").is_some());
  assert!(find_text(content2, "Alpha").is_none());
  assert!(find_text(footnote_area2, "Footnote two").is_some());
  assert!(find_text(footnote_area2, "Footnote one").is_none());
}

#[test]
fn footnote_overflow_defers_later_calls_to_next_page() {
  let html = r#"
    <html>
      <head>
        <style>
          @page { size: 200px 100px; margin: 0; }
          body { margin: 0; font-size: 10px; line-height: 10px; }
          p { margin: 0; }
          .note1 { float: footnote; display: inline-block; height: 10px; }
          .note2 { float: footnote; display: inline-block; height: 80px; }
        </style>
      </head>
      <body>
        <p>Alpha<span class="note1">Footnote one</span></p>
        <p>Beta<span class="note2">Footnote two</span></p>
      </body>
    </html>
  "#;

  let mut renderer = FastRender::new().unwrap();
  let dom = renderer.parse_html(html).unwrap();
  let tree = renderer
    .layout_document_for_media(&dom, 200, 100, MediaType::Print)
    .unwrap();
  let page_roots = pages(&tree);

  assert_eq!(
    page_roots.len(),
    2,
    "expected the second footnote to be deferred onto a new page"
  );

  let page1 = page_roots[0];
  let wrapper1 = page_document_wrapper(page1);
  assert_eq!(wrapper1.children.len(), 2);
  let content1 = page_content(page1);
  let footnote_area1 = wrapper1.children.get(1).expect("page 1 footnote area");

  assert!(find_text(content1, "Alpha").is_some());
  assert!(find_text(content1, "Beta").is_none());
  assert!(find_text(content1, "1").is_some());
  assert!(find_text(content1, "2").is_none());

  assert!(find_text(footnote_area1, "Footnote one").is_some());
  assert!(find_text(footnote_area1, "Footnote two").is_none());
  assert!(find_text(footnote_area1, "1").is_some());
  assert!(find_text(footnote_area1, "2").is_none());

  let page2 = page_roots[1];
  let wrapper2 = page_document_wrapper(page2);
  assert_eq!(wrapper2.children.len(), 2);
  let content2 = page_content(page2);
  let footnote_area2 = wrapper2.children.get(1).expect("page 2 footnote area");

  assert!(find_text(content2, "Beta").is_some());
  assert!(find_text(content2, "Alpha").is_none());
  assert!(find_text(content2, "2").is_some());
  assert!(find_text(content2, "1").is_none());

  assert!(find_text(footnote_area2, "Footnote two").is_some());
  assert!(find_text(footnote_area2, "Footnote one").is_none());
  assert!(find_text(footnote_area2, "2").is_some());
  assert!(find_text(footnote_area2, "1").is_none());
}

#[test]
fn huge_footnote_body_continues_across_pages() {
  let mut lines = String::new();
  for idx in 1..=40 {
    lines.push_str(&format!(r#"<span class="line">Footnote line {idx}</span>"#));
  }

  let html = format!(
    r#"
    <html>
      <head>
        <style>
          @page {{ size: 200px 100px; margin: 0; }}
          body {{ margin: 0; font-size: 10px; line-height: 10px; }}
          p {{ margin: 0; }}
          .note {{ float: footnote; }}
          .line {{ display: block; height: 10px; }}
        </style>
      </head>
      <body>
        <p>Main<span class="note">{lines}</span></p>
      </body>
    </html>
  "#
  );

  let mut renderer = FastRender::new().unwrap();
  let dom = renderer.parse_html(&html).unwrap();
  let tree = renderer
    .layout_document_for_media(&dom, 200, 100, MediaType::Print)
    .unwrap();
  let page_roots = pages(&tree);

  assert!(
    page_roots.len() >= 2,
    "expected huge footnote to create multiple pages, got {}",
    page_roots.len()
  );

  for (idx, page) in page_roots.iter().enumerate() {
    let wrapper = page_document_wrapper(page);
    assert_eq!(
      wrapper.children.len(),
      2,
      "page {} should include content + footnote area",
      idx + 1
    );
  }

  let page1 = page_roots[0];
  let content1 = page_content(page1);
  let footnote_area1 = page_document_wrapper(page1)
    .children
    .get(1)
    .expect("page 1 footnote area");
  assert!(find_text(content1, "Main").is_some());
  assert!(find_text(footnote_area1, "Footnote line 1").is_some());
  assert!(
    find_text(footnote_area1, "Footnote line 40").is_none(),
    "expected footnote to be fragmented (line 40 should not fit on page 1)"
  );

  let last = *page_roots.last().expect("last page");
  let last_footnote_area = page_document_wrapper(last)
    .children
    .get(1)
    .expect("last page footnote area");
  assert!(
    find_text(last_footnote_area, "Footnote line 40").is_some(),
    "expected last footnote line to appear on a later page"
  );
}

#[test]
fn footnote_only_continuation_pages_carry_running_strings_and_elements() {
  let mut lines = String::new();
  for idx in 1..=60 {
    lines.push_str(&format!(r#"<span class="line">Footnote line {idx}</span>"#));
  }

  let html = format!(
    r#"
    <html>
      <head>
        <style>
          @page {{
            size: 200px 140px;
            margin: 20px;
            @top-center {{ content: string(chapter) " - " element(header, start); }}
          }}

          body {{ margin: 0; font-size: 10px; line-height: 10px; }}
          h1 {{ margin: 0; string-set: chapter content(); }}
          h2 {{ position: running(header); margin: 0; }}
          p {{ margin: 0; }}

          .note {{ float: footnote; }}
          .line {{ display: block; height: 10px; }}
        </style>
      </head>
      <body>
        <h2>Header A</h2>
        <h1>Chapter A</h1>
        <p>Main<span class="note">{lines}</span></p>
      </body>
    </html>
  "#
  );

  let mut renderer = FastRender::new().unwrap();
  let dom = renderer.parse_html(&html).unwrap();
  let tree = renderer
    .layout_document_for_media(&dom, 200, 200, MediaType::Print)
    .unwrap();
  let page_roots = pages(&tree);

  assert!(
    page_roots.len() >= 2,
    "expected huge footnote to create multiple pages, got {}",
    page_roots.len()
  );

  let mut continuation_page = None;
  for page in page_roots.iter().skip(1) {
    let wrapper = page_document_wrapper(page);
    if wrapper.children.len() < 2 {
      continue;
    }
    // Footnote-only pages are emitted once main-flow content is exhausted. The content subtree is
    // still present, but should be empty.
    let content_text = collected_text_compacted(page_content(page));
    if !content_text.is_empty() {
      continue;
    }
    let footnote_area = wrapper.children.get(1).expect("footnote area");
    if !collected_text_compacted(footnote_area).is_empty() {
      continuation_page = Some(*page);
      break;
    }
  }

  let continuation_page = continuation_page.expect("expected a footnote-only continuation page");
  assert!(
    margin_boxes_contain_text(continuation_page, "Chapter A - Header A"),
    "expected footnote-only continuation page to carry running string/element values into margin boxes"
  );
}

#[test]
fn mixed_footnotes_preserve_order_when_one_is_huge() {
  let mut lines = String::new();
  for idx in 1..=40 {
    lines.push_str(&format!(
      r#"<span class="line">Footnote two line {idx}</span>"#
    ));
  }

  let html = format!(
    r#"
    <html>
      <head>
        <style>
          @page {{ size: 200px 100px; margin: 0; }}
          body {{ margin: 0; font-size: 10px; line-height: 10px; }}
          p {{ margin: 0; }}
          .note1 {{ float: footnote; display: inline-block; height: 10px; }}
          .note2 {{ float: footnote; }}
          .line {{ display: block; height: 10px; }}
        </style>
      </head>
      <body>
        <p>Alpha<span class="note1">Footnote one</span></p>
        <p>Beta<span class="note2">{lines}</span></p>
      </body>
    </html>
  "#
  );

  let mut renderer = FastRender::new().unwrap();
  let dom = renderer.parse_html(&html).unwrap();
  let tree = renderer
    .layout_document_for_media(&dom, 200, 100, MediaType::Print)
    .unwrap();
  let page_roots = pages(&tree);

  assert!(
    page_roots.len() >= 3,
    "expected mixed footnotes to require multiple pages, got {}",
    page_roots.len()
  );

  let page1 = page_roots[0];
  let wrapper1 = page_document_wrapper(page1);
  assert_eq!(wrapper1.children.len(), 2);
  let content1 = page_content(page1);
  let footnote_area1 = wrapper1.children.get(1).expect("page 1 footnote area");
  assert!(find_text(content1, "Alpha").is_some());
  assert!(find_text(content1, "Beta").is_none());
  assert!(find_text(footnote_area1, "Footnote one").is_some());
  assert!(find_text(footnote_area1, "Footnote two").is_none());

  let page2 = page_roots[1];
  let wrapper2 = page_document_wrapper(page2);
  assert_eq!(wrapper2.children.len(), 2);
  let content2 = page_content(page2);
  let footnote_area2 = wrapper2.children.get(1).expect("page 2 footnote area");
  assert!(find_text(content2, "Beta").is_some());
  assert!(find_text(content2, "Alpha").is_none());
  assert!(find_text(footnote_area2, "Footnote two line 1").is_some());
  assert!(find_text(footnote_area2, "Footnote one").is_none());

  let last = *page_roots.last().expect("last page");
  let last_footnote_area = page_document_wrapper(last)
    .children
    .get(1)
    .expect("last page footnote area");
  assert!(find_text(last_footnote_area, "Footnote two line 40").is_some());
  assert!(find_text(last_footnote_area, "Footnote one").is_none());
}

#[test]
fn box_decoration_break_clone_vs_slice_across_page_boundary() {
  const PAGE_WIDTH: u32 = 120;
  const PAGE_HEIGHT: u32 = 80;
  const BORDER_PX: u32 = 10;

  let border_rgba = Rgba::new(255, 0, 0, 1.0);
  let bg_rgba = Rgba::new(0, 255, 0, 1.0);
  let page_bg_rgba = Rgba::new(255, 255, 255, 1.0);

  let border_px = [255, 0, 0, 255];
  let bg_px = [0, 255, 0, 255];

  fn render_first_two_pages(
    box_decoration_break: &str,
    border_px: u32,
    border_rgba: Rgba,
    bg_rgba: Rgba,
    page_bg: Rgba,
  ) -> (resvg::tiny_skia::Pixmap, resvg::tiny_skia::Pixmap, FragmentNode, FragmentNode) {
    let html = format!(
      r#"
      <html>
        <head>
          <style>
            @page {{ size: {PAGE_WIDTH}px {PAGE_HEIGHT}px; margin: 0; background: rgb({page_r}, {page_g}, {page_b}); }}
            html, body {{ margin: 0; padding: 0; background: transparent; }}
            #box {{
              height: 140px;
              margin: 0;
              border: {border_px}px solid rgb({border_r}, {border_g}, {border_b});
              background: rgb({bg_r}, {bg_g}, {bg_b});
              background-clip: border-box;
              box-decoration-break: {box_decoration_break};
            }}
          </style>
        </head>
        <body>
          <div id="box"></div>
        </body>
      </html>
    "#
    ,
      page_r = page_bg.r,
      page_g = page_bg.g,
      page_b = page_bg.b,
      border_r = border_rgba.r,
      border_g = border_rgba.g,
      border_b = border_rgba.b,
      bg_r = bg_rgba.r,
      bg_g = bg_rgba.g,
      bg_b = bg_rgba.b,
    );

    let mut renderer = FastRender::new().unwrap();
    let dom = renderer.parse_html(&html).unwrap();
    let options = LayoutDocumentOptions::new().with_page_stacking(PageStacking::Untranslated);
    let tree = renderer
      .layout_document_for_media_with_options(
        &dom,
        PAGE_WIDTH,
        PAGE_HEIGHT,
        MediaType::Print,
        options,
        None,
      )
      .unwrap();
    let page_roots = pages(&tree);
    assert!(
      page_roots.len() >= 2,
      "expected at least two pages; got {}",
      page_roots.len()
    );

    let font_ctx = renderer.font_context().clone();
    let image_cache = ImageCache::new();
    let scroll_state = ScrollState::default();

    let render_page = |page: &FragmentNode| -> (resvg::tiny_skia::Pixmap, FragmentNode) {
      let offset = Point::new(-page.bounds.x(), -page.bounds.y());
      let translated_page = page.translate(offset);
      let viewport = translated_page.bounds.size;
      let page_tree = FragmentTree::with_viewport(translated_page.clone(), viewport);
      let pixmap = paint_tree_display_list_with_resources_scaled_offset(
        &page_tree,
        PAGE_WIDTH,
        PAGE_HEIGHT,
        page_bg,
        font_ctx.clone(),
        image_cache.clone(),
        1.0,
        Point::ZERO,
        PaintParallelism::disabled(),
        &scroll_state,
      )
      .expect("paint paged media page");
      (pixmap, translated_page)
    };

    let (pixmap1, page1) = render_page(page_roots[0]);
    let (pixmap2, page2) = render_page(page_roots[1]);
    (pixmap1, pixmap2, page1, page2)
  }

  for (mode, expect_border_on_page2) in [("clone", true), ("slice", false)] {
    let (page1_pixmap, page2_pixmap, page1_root, page2_root) = render_first_two_pages(
      mode,
      BORDER_PX,
      border_rgba,
      bg_rgba,
      page_bg_rgba,
    );

    let page1_box = find_fragment_by_background(&page1_root, bg_rgba).expect("box fragment page 1");
    let page2_box = find_fragment_by_background(&page2_root, bg_rgba).expect("box fragment page 2");

    let mid_x1 = (page1_box.bounds.x() + page1_box.bounds.width() / 2.0).round() as u32;
    let mid_x2 = (page2_box.bounds.x() + page2_box.bounds.width() / 2.0).round() as u32;
    let body_y1 = (page1_box.bounds.y() + BORDER_PX as f32 + 10.0).round() as u32;
    let body_y2 = (page2_box.bounds.y() + BORDER_PX as f32 + 10.0).round() as u32;
    let border_y2 = (page2_box.bounds.y() + (BORDER_PX as f32 / 2.0)).round() as u32;

    assert_eq!(
      pixel(&page1_pixmap, mid_x1, body_y1),
      bg_px,
      "expected box background to paint on page 1 for {mode}"
    );
    assert_eq!(
      pixel(&page2_pixmap, mid_x2, body_y2),
      bg_px,
      "expected box background to paint on page 2 for {mode}"
    );

    let page2_border_sample = pixel(&page2_pixmap, mid_x2, border_y2);
    if expect_border_on_page2 {
      assert_eq!(
        page2_border_sample, border_px,
        "expected cloned top border to paint on page 2 for {mode}"
      );
    } else {
      assert_eq!(
        page2_border_sample, bg_px,
        "expected sliced fragment edge to not paint a border on page 2 for {mode}"
      );
    }
  }
}
