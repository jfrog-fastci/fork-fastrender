use fastrender::api::FastRender;
use fastrender::style::display::Display;
use fastrender::tree::fragment_tree::FragmentNode;

fn find_table_wrapper_with_caption<'a>(node: &'a FragmentNode) -> Option<&'a FragmentNode> {
  if matches!(node.style.as_ref().map(|s| s.display), Some(Display::Table))
    && node.children.iter().any(|child| {
      matches!(
        child.style.as_ref().map(|s| s.display),
        Some(Display::TableCaption)
      )
    })
  {
    return Some(node);
  }
  node
    .children
    .iter()
    .find_map(find_table_wrapper_with_caption)
}

#[test]
fn table_caption_respects_explicit_width_and_auto_margins() {
  let html = r#"
    <html>
      <head>
        <style>
          body { margin: 0; }
          table { width: 200px; border-collapse: separate; border-spacing: 0; }
          table, caption, td { padding: 0; border: 0; }
          caption { width: 50px; margin: 0 auto; }
          td { height: 10px; }
        </style>
      </head>
      <body>
        <table>
          <caption>cap</caption>
          <tr><td></td></tr>
        </table>
      </body>
    </html>
  "#;

  let mut renderer = FastRender::new().unwrap();
  let dom = renderer.parse_html(html).unwrap();
  let tree = renderer.layout_document(&dom, 400, 200).unwrap();

  let wrapper = find_table_wrapper_with_caption(&tree.root).expect("table wrapper with caption");
  let caption = wrapper
    .children
    .iter()
    .find(|child| matches!(child.style.as_ref().map(|s| s.display), Some(Display::TableCaption)))
    .expect("caption fragment");

  assert!(
    (caption.bounds.width() - 50.0).abs() < 0.1,
    "caption width should respect explicit width (got {})",
    caption.bounds.width()
  );
  let expected_x = (wrapper.bounds.width() - caption.bounds.width()) / 2.0;
  assert!(
    (caption.bounds.x() - expected_x).abs() < 0.2,
    "caption should be horizontally centered by auto margins (x={}, expected {})",
    caption.bounds.x(),
    expected_x
  );
}

