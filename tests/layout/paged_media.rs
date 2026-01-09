use fastrender::api::{FastRender, LayoutDocumentOptions, PageStacking, RenderOptions};
use fastrender::style::media::MediaType;
use fastrender::tree::box_tree::ReplacedType;
use fastrender::tree::fragment_tree::{FragmentContent, FragmentNode, FragmentTree};
use fastrender::Rgba;
use regex::Regex;

fn pages<'a>(tree: &'a FragmentTree) -> Vec<&'a FragmentNode> {
  let mut roots = vec![&tree.root];
  roots.extend(tree.additional_fragments.iter());
  roots
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
    .map(|page| {
      let content = page.children.first().expect("page content");
      collected_text_compacted(content)
    })
    .collect()
}

fn collect_label_sequence(page_roots: &[&FragmentNode], re: &Regex) -> Vec<String> {
  let mut labels = Vec::new();
  for page in page_roots {
    let content = page.children.first().expect("page content");
    let text = collected_text_compacted(content);
    labels.extend(
      re.captures_iter(&text)
        .map(|cap| cap.get(1).expect("label group").as_str().to_string()),
    );
  }
  labels
}

fn count_text_fragments_by_column(page: &FragmentNode, needle: &str) -> (usize, usize) {
  let content = page.children.first().expect("page content");
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

fn assert_bounds_close(bounds: &fastrender::geometry::Rect, expected: (f32, f32, f32, f32)) {
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

  let content = page_roots[0].children.first().expect("page content");
  assert!((content.bounds.x() - 50.0).abs() < 0.1);
  assert!((content.bounds.y() - 20.0).abs() < 0.1);
  assert!((content.bounds.height() - 340.0).abs() < 0.1);
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
  let tree = renderer.layout_document_for_media(&dom, 400, 400, MediaType::Print).unwrap();
  let page = pages(&tree)[0];

  let content = page.children.first().expect("page content");
  assert!(
    (content.bounds.y() - 10.0).abs() < 0.1,
    "expected margin-top=10px from !important declaration; got {}",
    content.bounds.y()
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
  let tree = renderer.layout_document_for_media(&dom, 400, 400, MediaType::Print).unwrap();
  let page = pages(&tree)[0];
  let content = page.children.first().expect("page content");
  assert!(
    (content.bounds.y() - 20.0).abs() < 0.1,
    "expected later layer b to win for normal declarations; got {}",
    content.bounds.y()
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
  let tree = renderer.layout_document_for_media(&dom, 400, 400, MediaType::Print).unwrap();
  let page = pages(&tree)[0];
  let content = page.children.first().expect("page content");
  assert!(
    (content.bounds.y() - 10.0).abs() < 0.1,
    "expected earlier layer a to win for !important declarations; got {}",
    content.bounds.y()
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
  let first = page_roots[0].children.first().unwrap();
  let second = page_roots[1].children.first().unwrap();

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
  let tree = renderer.layout_document_for_media(&dom, 800, 600, MediaType::Print).unwrap();
  let page_roots = pages(&tree);

  assert!(
    page_roots.len() >= 4,
    "expected multiple pages with alternating sides; got {} pages",
    page_roots.len()
  );

  let width_right = page_roots[0].children.first().unwrap().bounds.width();
  let width_left = page_roots[1].children.first().unwrap().bounds.width();
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
  let content = page_roots[0].children.first().unwrap();
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
    preface.push_str(
      "preface words that wrap differently depending on the used page size and margins ",
    );
  }

  let mut chapter = String::new();
  for idx in 0..50 {
    chapter.push_str(&format!(r#"<span class="label">[C{idx:03}]</span> "#));
    chapter.push_str(
      "chapter words that wrap differently depending on the used page size and margins ",
    );
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
  let tree = renderer.layout_document_for_media(&dom, 800, 600, MediaType::Print).unwrap();
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
    let content = page.children.first().expect("page content");
    let text = collected_text_compacted(content);
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
    .position(|page| {
      let content = page.children.first().unwrap();
      find_text(content, "P049").is_some()
    })
    .expect("last preface label should exist");
  let chapter_first_page = page_roots
    .iter()
    .position(|page| {
      let content = page.children.first().unwrap();
      find_text(content, "C000").is_some()
    })
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

  let content = page.children.first().expect("content");
  let content_y = page.bounds.y() + content.bounds.y();
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
  let tree = renderer.layout_document_for_media(&dom, 400, 400, MediaType::Print).unwrap();
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
  let content = first_page.children.first().expect("page content");
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
  let content_y = first_page.bounds.y() + content.bounds.y();
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
  let content = first_page.children.first().expect("page content");
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
  let content_y = first_page.bounds.y() + content.bounds.y();
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
  let content = page.children.first().expect("page content");
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
  let content = page.children.first().expect("page content");

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
    let content_y = page.bounds.y()
      + page
        .children
        .first()
        .map(|n| n.bounds.y())
        .unwrap_or(f32::MAX);

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
  let tree = renderer.layout_document_for_media(&dom, 400, 400, MediaType::Print).unwrap();
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

  let content = page.children.first().expect("page content");
  let content_y = page.bounds.y() + content.bounds.y();

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
    let candidate_content_y = candidate.bounds.y()
      + candidate
        .children
        .first()
        .map(|n| n.bounds.y())
        .unwrap_or(f32::MAX);
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
    let candidate_content_y = candidate.bounds.y()
      + candidate
        .children
        .first()
        .map(|n| n.bounds.y())
        .unwrap_or(f32::MAX);
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
  let content_y = wide_page.bounds.y()
    + wide_page
      .children
      .first()
      .map(|n| n.bounds.y())
      .unwrap_or(f32::MAX);

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
  let tree = renderer.layout_document_for_media(&dom, 300, 200, MediaType::Print).unwrap();
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
  let tree = renderer.layout_document_for_media(&dom, 300, 200, MediaType::Print).unwrap();
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
  let tree = renderer.layout_document_for_media(&dom, 300, 200, MediaType::Print).unwrap();
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
  let tree = renderer.layout_document_for_media(&dom, 200, 200, MediaType::Print).unwrap();
  let page_roots = pages(&tree);

  assert_eq!(
    page_roots.len(),
    3,
    "expected 7 lines at 3 lines/page to paginate into 3 pages"
  );

  let lines = ["L1", "L2", "L3", "L4", "L5", "L6", "L7"];
  let mut seen = std::collections::HashSet::<&'static str>::new();

  for (idx, page) in page_roots.iter().enumerate() {
    let content = page.children.first().expect("page content");
    let text = collected_text_compacted(content);
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

  assert_eq!(seen.len(), lines.len(), "some lines were missing from output");
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
  let tree = renderer.layout_document_for_media(&dom, 200, 200, MediaType::Print).unwrap();
  let page_roots = pages(&tree);

  assert!(
    page_roots.len() >= 2,
    "expected avoid-page test content to paginate"
  );
  let first_content = page_roots[0].children.first().expect("page content");
  let first_text = collected_text_compacted(first_content);
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
  let tree = renderer.layout_document_for_media(&dom, 200, 200, MediaType::Print).unwrap();
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

  let float_bottom = second_page_floats
    .into_iter()
    .fold(0.0f32, f32::max);
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
  let float_bottom = second_page_floats
    .into_iter()
    .fold(0.0f32, f32::max);

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
  let float_bottom = second_page_floats
    .into_iter()
    .fold(0.0f32, f32::max);

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
    let content = page.children.first().expect("page content");
    let content_y = page.bounds.y() + content.bounds.y();

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
  let content = page.children.first().expect("page content");
  let content_y = page.bounds.y() + content.bounds.y();

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

  let first_content = page_roots[0].children.first().expect("page content");
  assert!(find_text(first_content, "Before").is_some());
  assert!(
    find_text(first_content, "After").is_none(),
    "later content should not appear on the first page"
  );

  let second_content = page_roots[1].children.first().expect("second page content");
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

  let first_content = page_roots[0].children.first().expect("page content");
  assert!(find_text(first_content, "Start").is_some());
  assert!(find_text(first_content, "Forced").is_none());

  let second_content = page_roots[1].children.first().expect("second page content");
  assert!(find_text(second_content, "Forced").is_some());
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
  assert_eq!(page1.children.len(), 2);

  let content = page1.children.first().expect("page content");
  let footnote_area = page1.children.get(1).expect("footnote area");

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
  assert_eq!(page1.children.len(), 2);

  let content = page1.children.first().expect("page content");
  let footnote_area = page1.children.get(1).expect("footnote area");

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
  assert_eq!(
    page1.children.len(),
    2,
    "page with footnote should have content + footnote area"
  );

  let footnote_area = page1.children.get(1).expect("footnote area");
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
  assert_eq!(
    page1.children.len(),
    2,
    "page with footnote should have content + footnote area"
  );
  let content = page1.children.first().expect("page content");
  let footnote_area = page1.children.get(1).expect("footnote area");

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
  assert_eq!(
    page2.children.len(),
    1,
    "pages without footnotes should not include a footnote area"
  );
  assert!(find_text(page2, "Page2").is_some());
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
  assert_eq!(
    page1.children.len(),
    2,
    "page with footnote should have content + footnote area"
  );
  let content = page1.children.first().expect("page content");
  let page_width = content.bounds.width();

  let footnote_area = page1.children.get(1).expect("footnote area");
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
  assert_eq!(page1.children.len(), 2);
  let content1 = page1.children.first().expect("page 1 content");
  let footnote_area1 = page1.children.get(1).expect("page 1 footnote area");

  assert!(find_text(content1, "Alpha").is_some());
  assert!(find_text(content1, "Beta").is_none());
  assert!(find_text(content1, "1").is_some());
  assert!(find_text(content1, "2").is_none());

  assert!(find_text(footnote_area1, "Footnote one").is_some());
  assert!(find_text(footnote_area1, "Footnote two").is_none());
  assert!(find_text(footnote_area1, "1").is_some());
  assert!(find_text(footnote_area1, "2").is_none());

  let page2 = page_roots[1];
  assert_eq!(page2.children.len(), 2);
  let content2 = page2.children.first().expect("page 2 content");
  let footnote_area2 = page2.children.get(1).expect("page 2 footnote area");

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
    assert_eq!(
      page.children.len(),
      2,
      "page {} should include content + footnote area",
      idx + 1
    );
  }

  let page1 = page_roots[0];
  let content1 = page1.children.first().expect("page 1 content");
  let footnote_area1 = page1.children.get(1).expect("page 1 footnote area");
  assert!(find_text(content1, "Main").is_some());
  assert!(find_text(footnote_area1, "Footnote line 1").is_some());
  assert!(
    find_text(footnote_area1, "Footnote line 40").is_none(),
    "expected footnote to be fragmented (line 40 should not fit on page 1)"
  );

  let last = *page_roots.last().expect("last page");
  let last_footnote_area = last.children.get(1).expect("last page footnote area");
  assert!(
    find_text(last_footnote_area, "Footnote line 40").is_some(),
    "expected last footnote line to appear on a later page"
  );
}

#[test]
fn mixed_footnotes_preserve_order_when_one_is_huge() {
  let mut lines = String::new();
  for idx in 1..=40 {
    lines.push_str(&format!(r#"<span class="line">Footnote two line {idx}</span>"#));
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
  assert_eq!(page1.children.len(), 2);
  let content1 = page1.children.first().expect("page 1 content");
  let footnote_area1 = page1.children.get(1).expect("page 1 footnote area");
  assert!(find_text(content1, "Alpha").is_some());
  assert!(find_text(content1, "Beta").is_none());
  assert!(find_text(footnote_area1, "Footnote one").is_some());
  assert!(find_text(footnote_area1, "Footnote two").is_none());

  let page2 = page_roots[1];
  assert_eq!(page2.children.len(), 2);
  let content2 = page2.children.first().expect("page 2 content");
  let footnote_area2 = page2.children.get(1).expect("page 2 footnote area");
  assert!(find_text(content2, "Beta").is_some());
  assert!(find_text(content2, "Alpha").is_none());
  assert!(find_text(footnote_area2, "Footnote two line 1").is_some());
  assert!(find_text(footnote_area2, "Footnote one").is_none());

  let last = *page_roots.last().expect("last page");
  let last_footnote_area = last.children.get(1).expect("last page footnote area");
  assert!(find_text(last_footnote_area, "Footnote two line 40").is_some());
  assert!(find_text(last_footnote_area, "Footnote one").is_none());
}
