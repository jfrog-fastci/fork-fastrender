use fastrender::api::{FastRender, LayoutDocumentOptions, PageStacking};
use fastrender::style::media::MediaType;
use fastrender::tree::fragment_tree::{FragmentContent, FragmentNode, FragmentTree};

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

#[test]
fn multicol_break_before_page_promotes_to_next_column_set() {
  let html = r#"
    <html>
      <head>
        <style>
          @page { size: 200px 140px; margin: 20px; }
          body { margin: 0; }
          .multi { column-count: 2; column-gap: 0; }
          .blk { height: 60px; margin: 0; }
          #b { break-before: page; }
        </style>
      </head>
      <body>
        <div class="multi">
          <div class="blk" id="a">A</div>
          <div class="blk" id="b">B</div>
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

  assert_eq!(page_roots.len(), 2);

  let page1 = page_roots[0];
  let page2 = page_roots[1];

  let content1 = page_content(page1);
  let content2 = page_content(page2);
  assert!(find_text(content1, "A").is_some());
  assert!(find_text(content1, "B").is_none());
  assert!(find_text(content2, "B").is_some());

  let pos_b = find_text_position(page2, "B", (0.0, 0.0)).expect("B on page 2");
  assert!(
    pos_b.1 <= page_content_start_y(page2) + 1.0,
    "expected B to start at the top of the second page; pos={pos_b:?} content_start_y={}",
    page_content_start_y(page2)
  );
}

#[test]
fn multicol_break_before_left_on_first_child_sets_first_page_side_ltr() {
  let html = r#"
    <html>
      <head>
        <style>
          @page { size: 200px 200px; margin: 20px; }
          @page :left { @top-center { content: "LEFT"; } }
          @page :right { @top-center { content: "RIGHT"; } }
          body { margin: 0; }
          .multi { column-count: 2; column-gap: 0; }
          .first { break-before: left; height: 80px; margin: 0; }
          .second { height: 80px; margin: 0; }
        </style>
      </head>
      <body>
        <div class="multi"><div class="first">A</div><div class="second">B</div></div>
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
    1,
    "forced start-side constraints should not insert leading blank pages"
  );
  let page = page_roots[0];
  assert!(find_text(page, "LEFT").is_some());
  assert!(find_text(page, "RIGHT").is_none());
  let content = page_content(page);
  assert!(find_text(content, "A").is_some());
  assert!(find_text(content, "B").is_some());
}

#[test]
fn multicol_break_after_recto_inserts_blank_page_rtl_progression() {
  let html = r#"
    <html>
      <head>
        <style>
          html { direction: rtl; }
          @page { size: 200px 200px; margin: 20px; }
          @page :blank { @top-center { content: "Blank"; } }
          body { margin: 0; }
          .multi { column-count: 2; column-gap: 0; }
          .blk { height: 80px; margin: 0; }
          #a { break-after: recto; }
        </style>
      </head>
      <body>
        <div class="multi">
          <div class="blk" id="a">A</div>
          <div class="blk" id="b">B</div>
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

  assert_eq!(page_roots.len(), 3);

  let page1 = page_roots[0];
  let blank_page = page_roots[1];
  let page3 = page_roots[2];

  let page1_content = page_content(page1);
  assert!(find_text(page1_content, "A").is_some());
  assert!(find_text(page1_content, "B").is_none());

  assert!(find_text(blank_page, "Blank").is_some());
  assert!(find_text(page_content(blank_page), "A").is_none());
  assert!(find_text(page_content(blank_page), "B").is_none());

  let page3_content = page_content(page3);
  assert!(find_text(page3_content, "B").is_some());
  let pos_b = find_text_position(page3, "B", (0.0, 0.0)).expect("B on page 3");
  assert!(
    pos_b.1 <= page_content_start_y(page3) + 1.0,
    "expected B to start at the top of the third page; pos={pos_b:?} content_start_y={}",
    page_content_start_y(page3)
  );
}

#[test]
fn multicol_break_before_right_on_first_child_sets_first_page_side_rtl() {
  let html = r#"
    <html>
      <head>
        <style>
          html { direction: rtl; }
          @page { size: 200px 200px; margin: 20px; }
          @page :left { @top-center { content: "LEFT"; } }
          @page :right { @top-center { content: "RIGHT"; } }
          body { margin: 0; }
          .multi { column-count: 2; column-gap: 0; }
          .first { break-before: right; height: 80px; margin: 0; }
          .second { height: 80px; margin: 0; }
        </style>
      </head>
      <body>
        <div class="multi"><div class="first">A</div><div class="second">B</div></div>
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
    1,
    "forced start-side constraints should not insert leading blank pages"
  );
  let page = page_roots[0];
  assert!(find_text(page, "RIGHT").is_some());
  assert!(find_text(page, "LEFT").is_none());
  let content = page_content(page);
  assert!(find_text(content, "A").is_some());
  assert!(find_text(content, "B").is_some());
}

#[test]
fn multicol_break_after_always_does_not_force_page_break() {
  let html = r#"
    <html>
      <head>
        <style>
          @page { size: 200px 200px; margin: 0; }
          body { margin: 0; }
          .multi { column-count: 2; column-gap: 0; }
          .blk { height: 150px; margin: 0; }
          #a { break-after: always; }
        </style>
      </head>
      <body>
        <div class="multi">
          <div class="blk" id="a">A</div>
          <div class="blk" id="b">B</div>
        </div>
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

  assert_eq!(
    page_roots.len(),
    1,
    "break-after: always inside paged multicol should force a column break, not a page break",
  );

  let page = page_roots[0];
  let a_pos = find_text_position(page, "A", (0.0, 0.0)).expect("page should contain A");
  let b_pos = find_text_position(page, "B", (0.0, 0.0)).expect("page should contain B");

  assert!(
    b_pos.0 > a_pos.0 + 0.5,
    "expected B to appear in the second column (A={a_pos:?}, B={b_pos:?})",
  );
}

