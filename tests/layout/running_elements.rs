use fastrender::api::FastRender;
use fastrender::geometry::Point;
use fastrender::style::media::MediaType;
use fastrender::tree::fragment_tree::{FragmentContent, FragmentNode, FragmentTree};

fn pages<'a>(tree: &'a FragmentTree) -> Vec<&'a FragmentNode> {
  let mut roots = vec![&tree.root];
  roots.extend(tree.additional_fragments.iter());
  roots
}

#[derive(Debug, Clone)]
struct PositionedText {
  text: String,
  x: f32,
  y: f32,
}

fn collect_text_fragments(node: &FragmentNode, origin: Point, out: &mut Vec<PositionedText>) {
  let abs_x = origin.x + node.bounds.x();
  let abs_y = origin.y + node.bounds.y();
  if let FragmentContent::Text { text, .. } = &node.content {
    out.push(PositionedText {
      text: text.to_string(),
      x: abs_x,
      y: abs_y,
    });
  }
  for child in node.children.iter() {
    collect_text_fragments(child, Point::new(abs_x, abs_y), out);
  }
}

fn collected_text_compacted(node: &FragmentNode) -> String {
  let mut texts = Vec::new();
  collect_text_fragments(node, Point::ZERO, &mut texts);
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

fn margin_texts(page: &FragmentNode) -> Vec<String> {
  page
    .children
    .iter()
    .skip(1)
    .map(collected_text_compacted)
    .collect()
}

#[test]
fn running_headers_follow_page_start() {
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
            h1 { position: running(header); margin: 0; font-size: 16px; }
            .spacer { height: 10px; }
            /* Force the second running element to start at the top of page 2. */
            .page-break { break-before: page; height: 0; }
          </style>
        </head>
        <body>
          <h1>First Title</h1>
          <div class="spacer"></div>
          <div class="page-break"></div>
          <h1>Second Title</h1>
          <div class="spacer"></div>
        </body>
      </html>
   "#;

  let mut renderer = FastRender::new().unwrap();
  let dom = renderer.parse_html(html).unwrap();
  let tree = renderer.layout_document_for_media(&dom, 400, 400, MediaType::Print).unwrap();
  let page_roots = pages(&tree);

  assert!(page_roots.len() >= 2);
  let first_margin = margin_texts(page_roots[0]);
  let second_margin = margin_texts(page_roots[1]);
  assert!(
    first_margin.iter().any(|t| t.contains("FirstTitle")),
    "first page header should use first running element"
  );
  assert!(
    second_margin.iter().any(|t| t.contains("SecondTitle")),
    "second page header should update when the running element starts the page (start selection)"
  );
}

#[test]
fn first_start_last_selection_differs() {
  let html = r#"
    <html>
      <head>
        <style>
          @page {
            size: 200px 200px;
            margin: 20px;
            @top-left { content: element(header); }
            @top-center { content: element(header, start); }
            @top-right { content: element(header, last); }
          }
          body { margin: 0; }
          h2 { position: running(header); margin: 0; font-size: 14px; }
          .fill { height: 210px; }
        </style>
      </head>
      <body>
        <h2>Alpha</h2>
        <div class="fill"></div>
        <h2>Beta</h2>
      </body>
    </html>
  "#;

  let mut renderer = FastRender::new().unwrap();
  let dom = renderer.parse_html(html).unwrap();
  let tree = renderer.layout_document_for_media(&dom, 400, 400, MediaType::Print).unwrap();
  let page_roots = pages(&tree);
  assert!(page_roots.len() >= 2);
  let second_page = page_roots[1];
  let texts = margin_texts(second_page);

  assert!(
    texts.iter().any(|t| t.contains("Alpha")),
    "element(header) should keep the first occurrence in document"
  );
  assert!(
    texts.iter().any(|t| t.contains("Beta")),
    "start/last selections should pick the running element on the current page"
  );
}

#[test]
fn running_elements_and_strings_coexist() {
  let html = r#"
    <html>
      <head>
        <style>
          h1 { string-set: chapter content(); }
          h2 { position: running(header); }
          @page {
            size: 200px 220px;
            margin: 20px;
            @top-center { content: string(chapter) " - " element(header, last); }
          }
          body { margin: 0; }
          .push { height: 210px; }
        </style>
      </head>
      <body>
        <h1>Chapter One</h1>
        <h2>Intro Header</h2>
        <div class="push"></div>
        <h2>Next Header</h2>
      </body>
    </html>
  "#;

  let mut renderer = FastRender::new().unwrap();
  let dom = renderer.parse_html(html).unwrap();
  let tree = renderer.layout_document_for_media(&dom, 400, 400, MediaType::Print).unwrap();
  let page_roots = pages(&tree);
  assert!(page_roots.len() >= 2);

  for page in page_roots {
    let texts = margin_texts(page);
    assert!(
      texts.iter().any(|t| t.contains("ChapterOne")),
      "string-set value should be available alongside running elements"
    );
    assert!(
      texts.iter().any(|t| t.contains("Header")),
      "running element content should render in the same margin box"
    );
  }
}

#[test]
fn first_except_suppresses_pages_with_occurrences_but_falls_back_when_missing() {
  let html = r#"
    <html>
      <head>
        <style>
          @page {
            size: 200px 200px;
            margin: 20px;
            @top-center { content: element(header, first-except); }
          }
          body { margin: 0; }
          h1 { position: running(header); margin: 0; font-size: 16px; }
          .fill { height: 260px; }
        </style>
      </head>
      <body>
        <h1>Carry Me</h1>
        <div class="fill"></div>
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

  let first_margin = margin_texts(page_roots[0]);
  let second_margin = margin_texts(page_roots[1]);

  assert!(
    !first_margin.iter().any(|t| t.contains("CarryMe")),
    "first-except should resolve to none on pages with an occurrence"
  );
  assert!(
    second_margin.iter().any(|t| t.contains("CarryMe")),
    "first-except should fall back to the carried running element when no occurrence exists"
  );
}

#[test]
fn running_header_start_selection_handles_vertical_rl_page_start() {
  let html = r#"
    <html>
      <head>
        <style>
          @page {
            size: 200px 200px;
            margin: 20px;
            @top-center { content: element(header, start); }
          }
          html { writing-mode: vertical-rl; }
          body { margin: 0; }
          h1 { position: running(header); margin: 0; font-size: 16px; }
          /* `width` maps to block-size in vertical writing modes. */
          .spacer { width: 20px; height: 10px; }
          .breaker { break-after: page; width: 10px; height: 10px; }
          .tail { width: 1px; height: 10px; }
        </style>
      </head>
      <body>
        <h1>One</h1>
        <div class="breaker"></div>
        <div class="spacer"></div>
        <h1>Two</h1>
        <div class="breaker"></div>
        <h1>Three</h1>
        <div class="tail"></div>
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
    page_roots.len() >= 3,
    "expected at least 3 pages for the forced breaks, got {}",
    page_roots.len()
  );

  let first_margin = margin_texts(page_roots[0]);
  let second_margin = margin_texts(page_roots[1]);
  let third_margin = margin_texts(page_roots[2]);

  assert!(
    first_margin.iter().any(|t| t.contains("One")),
    "page 1 should use the running header that starts the page"
  );
  assert!(
    second_margin.iter().any(|t| t.contains("One")) && !second_margin.iter().any(|t| t.contains("Two")),
    "page 2 header should remain the carried start value because the new running element is not at page start"
  );
  assert!(
    third_margin.iter().any(|t| t.contains("Three")),
    "page 3 should update when the running header starts the page in vertical-rl"
  );
}
