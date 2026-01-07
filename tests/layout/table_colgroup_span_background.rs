use fastrender::api::FastRender;
use fastrender::style::color::Rgba;
use fastrender::tree::fragment_tree::FragmentNode;

fn collect_fragments_with_bg<'a>(node: &'a FragmentNode, out: &mut Vec<&'a FragmentNode>) {
  let Some(style) = node.style.as_ref() else {
    for child in node.children.iter() {
      collect_fragments_with_bg(child, out);
    }
    return;
  };

  if style.background_color == Rgba::RED {
    out.push(node);
  }

  for child in node.children.iter() {
    collect_fragments_with_bg(child, out);
  }
}

#[test]
fn colgroup_span_background_covers_all_columns() {
  let html = r#"
    <html>
      <head>
        <style>
          table { border-collapse: collapse; border: none; table-layout: fixed; }
          colgroup { background-color: red; }
          td { border: none; width: 10px; height: 10px; padding: 0; margin: 0; }
        </style>
      </head>
      <body>
        <table style="width: 20px;">
          <colgroup span="2"></colgroup>
          <tr><td></td><td></td></tr>
        </table>
      </body>
    </html>
  "#;

  let mut renderer = FastRender::new().unwrap();
  let document = renderer.parse_html(html).unwrap();
  let tree = renderer.layout_document(&document, 200, 200).unwrap();

  let mut red_fragments = Vec::new();
  collect_fragments_with_bg(&tree.root, &mut red_fragments);
  assert_eq!(
    red_fragments.len(),
    1,
    "expected exactly one colgroup background fragment, found {}",
    red_fragments.len()
  );

  let width = red_fragments[0].bounds.width();
  assert!(
    (width - 20.0).abs() < 0.1,
    "expected colgroup background to span two 10px columns (~20px total), got {width}"
  );
}
