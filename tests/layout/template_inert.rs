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
fn template_contents_do_not_render_even_when_template_display_is_overridden() {
  let html = r#"
    <html>
      <head>
        <style>
          template { display: block; }
        </style>
      </head>
      <body>
        <template>
          <div>INERT</div>
        </template>
        <div>VISIBLE</div>
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

  assert!(joined.contains("VISIBLE"));
  assert!(
    !joined.contains("INERT"),
    "<template> contents are inert and must never render"
  );
}

