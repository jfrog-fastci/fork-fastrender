use fastrender::api::FastRender;
use fastrender::tree::fragment_tree::FragmentContent;

fn collect_text_fragments(node: &fastrender::FragmentNode, out: &mut Vec<String>) {
  let mut stack = vec![node];
  while let Some(node) = stack.pop() {
    if let FragmentContent::Text { text, .. } = &node.content {
      out.push(text.to_string());
    }
    for child in node.children.iter().rev() {
      stack.push(child);
    }
  }
}

#[test]
fn closed_details_hides_details_contents_including_text_and_extra_summaries() {
  let html = r#"
    <html>
      <body>
        <details>
          <summary>Title</summary>
          Hidden
          <summary>Extra</summary>
          <div>More</div>
        </details>
      </body>
    </html>
  "#;

  let mut renderer = FastRender::new().expect("renderer");
  let dom = renderer.parse_html(html).expect("dom");
  let tree = renderer
    .layout_document(&dom, 400, 200)
    .expect("layout should succeed");

  let mut texts = Vec::new();
  collect_text_fragments(&tree.root, &mut texts);
  let joined = texts.join(" ");

  assert!(joined.contains("Title"));
  assert!(
    !joined.contains("Hidden"),
    "closed <details> must suppress direct text-node contents"
  );
  assert!(
    !joined.contains("Extra"),
    "only the first <summary> should render when <details> is closed"
  );
  assert!(
    !joined.contains("More"),
    "closed <details> must hide non-summary contents"
  );
}

#[test]
fn open_details_renders_details_contents_including_text_and_extra_summaries() {
  let html = r#"
    <html>
      <body>
        <details open>
          <summary>Title</summary>
          Hidden
          <summary>Extra</summary>
          <div>More</div>
        </details>
      </body>
    </html>
  "#;

  let mut renderer = FastRender::new().expect("renderer");
  let dom = renderer.parse_html(html).expect("dom");
  let tree = renderer
    .layout_document(&dom, 400, 200)
    .expect("layout should succeed");

  let mut texts = Vec::new();
  collect_text_fragments(&tree.root, &mut texts);
  let joined = texts.join(" ");

  assert!(joined.contains("Title"));
  assert!(joined.contains("Hidden"));
  assert!(joined.contains("Extra"));
  assert!(joined.contains("More"));
}

