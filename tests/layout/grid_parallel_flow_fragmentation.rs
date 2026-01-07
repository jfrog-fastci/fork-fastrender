use fastrender::api::FastRender;
use fastrender::style::media::MediaType;
use fastrender::tree::fragment_tree::{FragmentContent, FragmentNode, FragmentTree};

fn pages<'a>(tree: &'a FragmentTree) -> Vec<&'a FragmentNode> {
  let mut roots = vec![&tree.root];
  roots.extend(tree.additional_fragments.iter());
  roots
}

fn collect_text(node: &FragmentNode, out: &mut String) {
  if let FragmentContent::Text { text, .. } = &node.content {
    out.push_str(text);
  }
  for child in node.children.iter() {
    collect_text(child, out);
  }
}

fn page_text(page: &FragmentNode) -> String {
  let mut out = String::new();
  collect_text(page, &mut out);
  out.retain(|c| !c.is_whitespace());
  out
}

#[test]
fn forced_break_inside_grid_item_does_not_force_siblings() {
  let html = r#"
    <html>
      <head>
        <style>
          @page { size: 200px 200px; margin: 0; }
          html, body { margin: 0; }
          .grid {
            display: grid;
            grid-template-columns: 1fr 1fr;
            align-items: end;
            font: 16px/16px sans-serif;
          }
          .a1, .a2 { height: 80px; }
          .a2 { break-before: page; }
        </style>
      </head>
      <body>
        <div class="grid">
          <div>
            <div class="a1">A1</div>
            <div class="a2">A2</div>
          </div>
          <div>B</div>
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

  assert!(page_roots.len() >= 2, "forced break should create multiple pages");

  let page1 = page_roots[0].children.first().expect("page 1 content");
  let page2 = page_roots[1].children.first().expect("page 2 content");

  let page1_text = page_text(page1);
  let page2_text = page_text(page2);

  assert!(page1_text.contains("A1"));
  assert!(page1_text.contains("B"));
  assert!(!page1_text.contains("A2"));

  assert!(page2_text.contains("A2"));
  assert!(!page2_text.contains("B"));
}

#[test]
fn grid_container_adds_pages_for_item_continuations() {
  let html = r#"
    <html>
      <head>
        <style>
          @page { size: 200px 200px; margin: 0; }
          html, body { margin: 0; }
          .grid {
            display: grid;
            grid-template-columns: 1fr 1fr;
            font: 16px/16px sans-serif;
          }
          .a1, .a2 { height: 20px; }
          .a2 { break-before: page; }
        </style>
      </head>
      <body>
        <div class="grid">
          <div>
            <div class="a1">A1</div>
            <div class="a2">A2</div>
          </div>
          <div> B </div>
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
    "grid container should paginate to show a continuing grid item"
  );

  let page2 = page_roots[1].children.first().expect("page 2 content");
  let page2_text = page_text(page2);
  assert!(
    page2_text.contains("A2"),
    "continuation text should appear on the second page"
  );
}
