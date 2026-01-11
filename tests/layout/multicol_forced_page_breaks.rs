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
  let wrapper = page_document_wrapper(page);
  wrapper.children.first().unwrap_or(wrapper)
}

fn page_content_start_y(page: &FragmentNode) -> f32 {
  let wrapper = page_document_wrapper(page);
  let content_y = wrapper
    .children
    .first()
    .map(|node| node.bounds.y())
    .unwrap_or(0.0);
  page.bounds.y() + wrapper.bounds.y() + content_y
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

fn margin_boxes_contain_text(page: &FragmentNode, needle: &str) -> bool {
  page
    .children
    .iter()
    .skip(1)
    .any(|child| find_text(child, needle).is_some())
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
  assert!(find_text_eq(content1, "A").is_some());
  assert!(find_text_eq(content1, "B").is_none());
  assert!(find_text_eq(content2, "B").is_some());

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
  assert!(find_text_eq(content, "A").is_some());
  assert!(find_text_eq(content, "B").is_some());
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
  assert!(find_text_eq(page1_content, "A").is_some());
  assert!(find_text_eq(page1_content, "B").is_none());

  assert!(find_text(blank_page, "Blank").is_some());
  assert!(find_text_eq(blank_page, "A").is_none());
  assert!(find_text_eq(blank_page, "B").is_none());

  let page3_content = page_content(page3);
  assert!(find_text_eq(page3_content, "B").is_some());
  let pos_b = find_text_position(page3, "B", (0.0, 0.0)).expect("B on page 3");
  assert!(
    pos_b.1 <= page_content_start_y(page3) + 1.0,
    "expected B to start at the top of the third page; pos={pos_b:?} content_start_y={}",
    page_content_start_y(page3)
  );
}

#[test]
fn multicol_break_after_verso_inserts_blank_page_rtl() {
  let html = r#"
    <html>
      <head>
        <style>
          html { direction: rtl; }
          @page { size: 200px 200px; margin: 20px; }
          @page :blank { @top-center { content: "Blank"; } }
          body { margin: 0; }
          .pre { height: 160px; margin: 0; }
          .multi { column-count: 2; column-gap: 0; }
          .blk { height: 80px; margin: 0; }
          #a { break-after: verso; }
        </style>
      </head>
      <body>
        <div class="pre">Pre</div>
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

  assert_eq!(page_roots.len(), 4);

  let page1 = page_roots[0];
  let page2 = page_roots[1];
  let blank_page = page_roots[2];
  let page4 = page_roots[3];

  assert!(find_text_eq(page1, "Pre").is_some());
  assert!(find_text_eq(page1, "A").is_none());
  assert!(find_text_eq(page1, "B").is_none());

  assert!(find_text_eq(page2, "A").is_some());
  assert!(find_text_eq(page2, "B").is_none());

  assert!(find_text(blank_page, "Blank").is_some());
  assert!(find_text_eq(blank_page, "Pre").is_none());
  assert!(find_text_eq(blank_page, "A").is_none());
  assert!(find_text_eq(blank_page, "B").is_none());

  let pos_b = find_text_position(page4, "B", (0.0, 0.0)).expect("B on page 4");
  assert!(
    pos_b.1 <= page_content_start_y(page4) + 1.0,
    "expected B to start at the page content top; pos={pos_b:?} content_start_y={}",
    page_content_start_y(page4)
  );
}

#[test]
fn multicol_break_after_recto_inserts_blank_page_ltr() {
  let html = r#"
     <html>
       <head>
         <style>
           html { direction: ltr; }
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
  let options = LayoutDocumentOptions::new().with_page_stacking(PageStacking::Untranslated);
  let tree = renderer
    .layout_document_for_media_with_options(&dom, 400, 400, MediaType::Print, options, None)
    .unwrap();
  let page_roots = pages(&tree);

  assert_eq!(page_roots.len(), 3);

  let page1 = page_roots[0];
  let blank_page = page_roots[1];
  let page3 = page_roots[2];

  let page1_content = page_content(page1);
  assert!(find_text_eq(page1_content, "A").is_some());
  assert!(find_text_eq(page1_content, "B").is_none());
  assert!(find_text(page1, "Blank").is_none());

  assert!(find_text(blank_page, "Blank").is_some());
  assert!(find_text_eq(blank_page, "A").is_none());
  assert!(find_text_eq(blank_page, "B").is_none());

  let page3_content = page_content(page3);
  assert!(find_text_eq(page3_content, "B").is_some());
  assert!(find_text(page3, "Blank").is_none());
  let pos_b = find_text_position(page3, "B", (0.0, 0.0)).expect("B on page 3");
  assert!(
    pos_b.1 <= page_content_start_y(page3) + 1.0,
    "expected B to start at the top of the third page; pos={pos_b:?} content_start_y={}",
    page_content_start_y(page3)
  );
}

#[test]
fn multicol_recto_mapping_uses_root_page_progression_not_container_direction() {
  let html = r#"
     <html>
       <head>
         <style>
           html { direction: ltr; }
           @page { size: 200px 200px; margin: 20px; }
           @page :blank { @top-center { content: "Blank"; } }
           body { margin: 0; }
           .multi { direction: rtl; column-count: 2; column-gap: 0; }
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
  let options = LayoutDocumentOptions::new().with_page_stacking(PageStacking::Untranslated);
  let tree = renderer
    .layout_document_for_media_with_options(&dom, 400, 400, MediaType::Print, options, None)
    .unwrap();
  let page_roots = pages(&tree);

  assert_eq!(page_roots.len(), 3);

  let page1 = page_roots[0];
  let blank_page = page_roots[1];
  let page3 = page_roots[2];

  let page1_content = page_content(page1);
  assert!(find_text_eq(page1_content, "A").is_some());
  assert!(find_text_eq(page1_content, "B").is_none());
  assert!(find_text(page1, "Blank").is_none());

  assert!(find_text(blank_page, "Blank").is_some());
  assert!(find_text_eq(blank_page, "A").is_none());
  assert!(find_text_eq(blank_page, "B").is_none());

  let page3_content = page_content(page3);
  assert!(find_text_eq(page3_content, "B").is_some());
  assert!(find_text(page3, "Blank").is_none());
  let pos_b = find_text_position(page3, "B", (0.0, 0.0)).expect("B on page 3");
  assert!(
    pos_b.1 <= page_content_start_y(page3) + 1.0,
    "expected B to start at the top of the third page; pos={pos_b:?} content_start_y={}",
    page_content_start_y(page3)
  );
}

#[test]
fn multicol_break_after_verso_inserts_blank_page_ltr() {
  let html = r#"
    <html>
      <head>
        <style>
          html { direction: ltr; }
          @page { size: 200px 200px; margin: 20px; }
          @page :blank { @top-center { content: "Blank"; } }
          body { margin: 0; }
          .pre { height: 160px; margin: 0; }
          .multi { column-count: 2; column-gap: 0; }
          .blk { height: 80px; margin: 0; }
          #a { break-after: verso; }
        </style>
      </head>
      <body>
        <div class="pre">Pre</div>
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

  assert_eq!(page_roots.len(), 4);

  let page1 = page_roots[0];
  let page2 = page_roots[1];
  let blank_page = page_roots[2];
  let page4 = page_roots[3];

  let page1_content = page_content(page1);
  assert!(find_text(page1_content, "Pre").is_some());
  assert!(find_text(page1_content, "A").is_none());
  assert!(find_text(page1_content, "B").is_none());
  assert!(!margin_boxes_contain_text(page1, "Blank"));

  let page2_content = page_content(page2);
  assert!(find_text(page2_content, "A").is_some());
  assert!(find_text(page2_content, "B").is_none());
  assert!(!margin_boxes_contain_text(page2, "Blank"));

  assert!(
    margin_boxes_contain_text(blank_page, "Blank"),
    "blank page should use the :blank page rule"
  );
  let blank_page_content = page_content(blank_page);
  assert!(find_text(blank_page_content, "Pre").is_none());
  assert!(find_text(blank_page_content, "A").is_none());
  assert!(find_text(blank_page_content, "B").is_none());

  let page4_content = page_content(page4);
  assert!(find_text(page4_content, "B").is_some());
  assert!(!margin_boxes_contain_text(page4, "Blank"));

  let pos_b = find_text_position(page4, "B", (0.0, 0.0)).expect("B on page 4");
  assert!(
    pos_b.1 <= page_content_start_y(page4) + 1.0,
    "expected B to start at the top of the fourth page; pos={pos_b:?} content_start_y={}",
    page_content_start_y(page4)
  );
}

#[test]
fn multicol_break_after_verso_does_not_insert_blank_page_ltr() {
  let html = r#"
    <html>
      <head>
        <style>
          html { direction: ltr; }
          @page { size: 200px 200px; margin: 20px; }
          @page :left { @top-center { content: "LEFT"; } }
          @page :right { @top-center { content: "RIGHT"; } }
          @page :blank { @top-center { content: "Blank"; } }
          body { margin: 0; }
          .multi { column-count: 2; column-gap: 0; }
          .blk { height: 80px; margin: 0; }
          #a { break-after: verso; }
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

  assert!(margin_boxes_contain_text(page1, "RIGHT"));
  assert!(!margin_boxes_contain_text(page1, "LEFT"));
  assert!(!margin_boxes_contain_text(page1, "Blank"));

  let page1_content = page_content(page1);
  assert!(find_text_eq(page1_content, "A").is_some());
  assert!(find_text_eq(page1_content, "B").is_none());

  assert!(margin_boxes_contain_text(page2, "LEFT"));
  assert!(!margin_boxes_contain_text(page2, "RIGHT"));
  assert!(!margin_boxes_contain_text(page2, "Blank"));

  let page2_content = page_content(page2);
  assert!(find_text_eq(page2_content, "A").is_none());
  assert!(find_text_eq(page2_content, "B").is_some());

  let pos_b = find_text_position(page2, "B", (0.0, 0.0)).expect("B on page 2");
  assert!(
    pos_b.1 <= page_content_start_y(page2) + 1.0,
    "expected B to start at the top of the second page; pos={pos_b:?} content_start_y={}",
    page_content_start_y(page2)
  );
}

#[test]
fn multicol_break_after_recto_does_not_insert_blank_page_ltr() {
  let html = r#"
    <html>
      <head>
        <style>
          html { direction: ltr; }
          @page { size: 200px 200px; margin: 20px; }
          @page :left { @top-center { content: "LEFT"; } }
          @page :right { @top-center { content: "RIGHT"; } }
          @page :blank { @top-center { content: "Blank"; } }
          body { margin: 0; }
          .pre { height: 160px; margin: 0; }
          .multi { column-count: 2; column-gap: 0; }
          .blk { height: 80px; margin: 0; }
          #a { break-after: recto; }
        </style>
      </head>
      <body>
        <div class="pre">Pre</div>
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

  assert_eq!(page_roots.len(), 3);

  let page1 = page_roots[0];
  let page2 = page_roots[1];
  let page3 = page_roots[2];

  assert!(margin_boxes_contain_text(page1, "RIGHT"));
  assert!(!margin_boxes_contain_text(page1, "LEFT"));
  assert!(!margin_boxes_contain_text(page1, "Blank"));
  let page1_content = page_content(page1);
  assert!(find_text_eq(page1_content, "Pre").is_some());
  assert!(find_text_eq(page1_content, "A").is_none());
  assert!(find_text_eq(page1_content, "B").is_none());

  assert!(margin_boxes_contain_text(page2, "LEFT"));
  assert!(!margin_boxes_contain_text(page2, "RIGHT"));
  assert!(!margin_boxes_contain_text(page2, "Blank"));
  let page2_content = page_content(page2);
  assert!(find_text_eq(page2_content, "Pre").is_none());
  assert!(find_text_eq(page2_content, "A").is_some());
  assert!(find_text_eq(page2_content, "B").is_none());

  assert!(margin_boxes_contain_text(page3, "RIGHT"));
  assert!(!margin_boxes_contain_text(page3, "LEFT"));
  assert!(!margin_boxes_contain_text(page3, "Blank"));
  let page3_content = page_content(page3);
  assert!(find_text_eq(page3_content, "Pre").is_none());
  assert!(find_text_eq(page3_content, "A").is_none());
  assert!(find_text_eq(page3_content, "B").is_some());

  let pos_b = find_text_position(page3, "B", (0.0, 0.0)).expect("B on page 3");
  assert!(
    pos_b.1 <= page_content_start_y(page3) + 1.0,
    "expected B to start at the top of the third page; pos={pos_b:?} content_start_y={}",
    page_content_start_y(page3)
  );
}

#[test]
fn multicol_break_after_recto_inserts_blank_page_ltr_progression() {
  let html = r#"
     <html>
       <head>
        <style>
          @page { size: 200px 200px; margin: 20px; }
          @page :blank { @top-center { content: "Blank"; } }
          body { margin: 0; }
          .prelude { break-before: left; break-after: page; height: 80px; margin: 0; }
          .multi { column-count: 2; column-gap: 0; }
          .blk { height: 80px; margin: 0; }
          #a { break-after: recto; }
        </style>
      </head>
      <body>
        <div class="prelude">Prelude</div>
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

  assert_eq!(page_roots.len(), 4);

  let page1 = page_roots[0];
  let page2 = page_roots[1];
  let blank_page = page_roots[2];
  let page4 = page_roots[3];

  assert!(find_text(page_content(page1), "Prelude").is_some());
  assert!(!margin_boxes_contain_text(page1, "Blank"));

  let page2_content = page_content(page2);
  assert!(find_text(page2_content, "A").is_some());
  assert!(find_text(page2_content, "B").is_none());
  assert!(!margin_boxes_contain_text(page2, "Blank"));

  assert!(
    margin_boxes_contain_text(blank_page, "Blank"),
    "blank page should use the :blank page rule"
  );
  let blank_page_content = page_content(blank_page);
  assert!(find_text(blank_page_content, "Prelude").is_none());
  assert!(find_text(blank_page_content, "A").is_none());
  assert!(find_text(blank_page_content, "B").is_none());

  let page4_content = page_content(page4);
  assert!(find_text(page4_content, "A").is_none());
  assert!(find_text(page4_content, "B").is_some());
  assert!(!margin_boxes_contain_text(page4, "Blank"));

  let pos_b = find_text_position(page4, "B", (0.0, 0.0)).expect("B on page 4");
  assert!(
    pos_b.1 <= page_content_start_y(page4) + 1.0,
    "expected B to start at the top of the fourth page; pos={pos_b:?} content_start_y={}",
    page_content_start_y(page4)
  );
}

#[test]
fn multicol_break_after_verso_inserts_blank_page_ltr_progression() {
  let html = r#"
    <html>
      <head>
        <style>
          @page { size: 200px 200px; margin: 20px; }
          @page :blank { @top-center { content: "Blank"; } }
          body { margin: 0; }
          .prelude { break-after: page; height: 80px; margin: 0; }
          .multi { column-count: 2; column-gap: 0; }
          .blk { height: 80px; margin: 0; }
          #a { break-after: verso; }
        </style>
      </head>
      <body>
        <div class="prelude">Prelude</div>
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

  assert_eq!(page_roots.len(), 4);

  let page1 = page_roots[0];
  let page2 = page_roots[1];
  let blank_page = page_roots[2];
  let page4 = page_roots[3];

  assert!(find_text(page_content(page1), "Prelude").is_some());
  assert!(!margin_boxes_contain_text(page1, "Blank"));

  let page2_content = page_content(page2);
  assert!(find_text(page2_content, "A").is_some());
  assert!(find_text(page2_content, "B").is_none());
  assert!(!margin_boxes_contain_text(page2, "Blank"));

  assert!(
    margin_boxes_contain_text(blank_page, "Blank"),
    "blank page should use the :blank page rule"
  );
  let blank_page_content = page_content(blank_page);
  assert!(find_text(blank_page_content, "Prelude").is_none());
  assert!(find_text(blank_page_content, "A").is_none());
  assert!(find_text(blank_page_content, "B").is_none());

  let page4_content = page_content(page4);
  assert!(find_text(page4_content, "A").is_none());
  assert!(find_text(page4_content, "B").is_some());
  assert!(!margin_boxes_contain_text(page4, "Blank"));

  let pos_b = find_text_position(page4, "B", (0.0, 0.0)).expect("B on page 4");
  assert!(
    pos_b.1 <= page_content_start_y(page4) + 1.0,
    "expected B to start at the top of the fourth page; pos={pos_b:?} content_start_y={}",
    page_content_start_y(page4)
  );
}

#[test]
fn multicol_break_after_verso_inserts_blank_page_rtl_progression() {
  let html = r#"
    <html>
      <head>
        <style>
          html { direction: rtl; }
          @page { size: 200px 200px; margin: 20px; }
          @page :blank { @top-center { content: "Blank"; } }
          body { margin: 0; }
          .prelude { break-after: page; height: 80px; margin: 0; }
          .multi { column-count: 2; column-gap: 0; }
          .blk { height: 80px; margin: 0; }
          #a { break-after: verso; }
        </style>
      </head>
      <body>
        <div class="prelude">Prelude</div>
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

  assert_eq!(page_roots.len(), 4);

  let page1 = page_roots[0];
  let page2 = page_roots[1];
  let blank_page = page_roots[2];
  let page4 = page_roots[3];

  assert!(find_text(page_content(page1), "Prelude").is_some());
  assert!(!margin_boxes_contain_text(page1, "Blank"));

  let page2_content = page_content(page2);
  assert!(find_text(page2_content, "A").is_some());
  assert!(find_text(page2_content, "B").is_none());
  assert!(!margin_boxes_contain_text(page2, "Blank"));

  assert!(
    margin_boxes_contain_text(blank_page, "Blank"),
    "blank page should use the :blank page rule"
  );
  let blank_page_content = page_content(blank_page);
  assert!(find_text(blank_page_content, "Prelude").is_none());
  assert!(find_text(blank_page_content, "A").is_none());
  assert!(find_text(blank_page_content, "B").is_none());

  let page4_content = page_content(page4);
  assert!(find_text(page4_content, "A").is_none());
  assert!(find_text(page4_content, "B").is_some());
  assert!(!margin_boxes_contain_text(page4, "Blank"));

  let pos_b = find_text_position(page4, "B", (0.0, 0.0)).expect("B on page 4");
  assert!(
    pos_b.1 <= page_content_start_y(page4) + 1.0,
    "expected B to start at the top of the fourth page; pos={pos_b:?} content_start_y={}",
    page_content_start_y(page4)
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
  assert!(find_text_eq(content, "A").is_some());
  assert!(find_text_eq(content, "B").is_some());
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

#[test]
fn multicol_break_before_always_does_not_force_page_break() {
  let html = r#"
    <html>
      <head>
        <style>
          @page { size: 200px 200px; margin: 0; }
          body { margin: 0; }
          .multi { column-count: 2; column-gap: 0; }
          .blk { height: 150px; margin: 0; }
          #b { break-before: always; }
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
    "break-before: always inside paged multicol should force a column break, not a page break",
  );

  let page = page_roots[0];
  let a_pos = find_text_position(page, "A", (0.0, 0.0)).expect("page should contain A");
  let b_pos = find_text_position(page, "B", (0.0, 0.0)).expect("page should contain B");

  assert!(
    b_pos.0 > a_pos.0 + 0.5,
    "expected B to appear in the second column (A={a_pos:?}, B={b_pos:?})",
  );
}

#[test]
fn multicol_break_after_page_before_spanner_does_not_create_extra_column_set() {
  let html = r#"
    <html>
      <head>
        <style>
          @page { size: 200px 140px; margin: 20px; }
          body { margin: 0; }
          .multi { column-count: 2; column-gap: 0; }
          .blk { margin: 0; }
          #a { height: 60px; }
          #b { height: 20px; break-after: page; }
          .spanner { column-span: all; height: 10px; margin: 0; }
        </style>
      </head>
      <body>
        <div class="multi"><div class="blk" id="a">A</div><div class="blk" id="b">B</div><div class="spanner">S</div></div>
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
    "break-after: page before a spanner should not introduce an extra empty column set"
  );

  let page1 = page_roots[0];
  let page2 = page_roots[1];
  let page1_content = page_content(page1);
  let page2_content = page_content(page2);
  assert!(find_text_eq(page1_content, "A").is_some());
  assert!(find_text_eq(page1_content, "B").is_some());
  assert!(find_text_eq(page1_content, "S").is_none());
  assert!(find_text_eq(page2_content, "S").is_some());

  let pos_s = find_text_position(page2, "S", (0.0, 0.0)).expect("S on page 2");
  assert!(
    pos_s.1 <= page_content_start_y(page2) + 1.0,
    "expected spanner to start at the top of the second page; pos={pos_s:?} content_start_y={}",
    page_content_start_y(page2)
  );
}

#[test]
fn multicol_break_after_right_before_spanner_inserts_single_blank_page() {
  let html = r#"
    <html>
      <head>
        <style>
          @page { size: 200px 140px; margin: 20px; }
          @page :blank { @top-center { content: "Blank"; } }
          body { margin: 0; }
          .multi { column-count: 2; column-gap: 0; }
          .blk { margin: 0; }
          #a { height: 60px; }
          #b { height: 20px; break-after: right; }
          .spanner { column-span: all; height: 10px; margin: 0; }
        </style>
      </head>
      <body>
        <div class="multi"><div class="blk" id="a">A</div><div class="blk" id="b">B</div><div class="spanner">S</div></div>
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
    "break-after: right before a spanner should insert exactly one blank page"
  );

  let page1 = page_roots[0];
  let blank_page = page_roots[1];
  let page3 = page_roots[2];

  let page1_content = page_content(page1);
  assert!(find_text_eq(page1_content, "A").is_some());
  assert!(find_text_eq(page1_content, "B").is_some());
  assert!(find_text_eq(page1_content, "S").is_none());

  assert!(find_text(blank_page, "Blank").is_some());
  assert!(find_text_eq(blank_page, "A").is_none());
  assert!(find_text_eq(blank_page, "B").is_none());
  assert!(find_text_eq(blank_page, "S").is_none());

  let page3_content = page_content(page3);
  assert!(find_text_eq(page3_content, "S").is_some());
  let pos_s = find_text_position(page3, "S", (0.0, 0.0)).expect("S on page 3");
  assert!(
    pos_s.1 <= page_content_start_y(page3) + 1.0,
    "expected spanner to start at the top of the third page; pos={pos_s:?} content_start_y={}",
    page_content_start_y(page3)
  );
}
