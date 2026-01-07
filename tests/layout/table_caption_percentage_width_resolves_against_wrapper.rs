use fastrender::api::FastRender;
use fastrender::style::display::Display;
use fastrender::tree::fragment_tree::FragmentNode;

fn is_table_like(display: Display) -> bool {
  matches!(display, Display::Table | Display::InlineTable)
}

fn find_table_wrapper_with_caption<'a>(node: &'a FragmentNode) -> Option<&'a FragmentNode> {
  if node
    .style
    .as_ref()
    .is_some_and(|style| is_table_like(style.display))
    && node.children.iter().any(|child| {
      matches!(
        child.style.as_ref().map(|s| s.display),
        Some(Display::TableCaption)
      )
    })
  {
    return Some(node);
  }
  node.children.iter().find_map(find_table_wrapper_with_caption)
}

#[test]
fn table_caption_percentage_width_resolves_against_wrapper() {
  let html = r#"
    <html>
      <head>
        <style>
          body { margin: 0; }
          table { width: 200px; border-collapse: separate; border-spacing: 0; }
          table, caption, td { padding: 0; border: 0; }
          caption { width: 50%; }
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
    (wrapper.bounds.width() - 200.0).abs() < 0.1,
    "wrapper should match the table width (got {})",
    wrapper.bounds.width()
  );
  assert!(
    (caption.bounds.width() - 100.0).abs() < 0.5,
    "caption width:50% should resolve against wrapper width (got {})",
    caption.bounds.width()
  );
  assert!(
    caption.bounds.x().abs() < 0.1,
    "caption should be left-aligned when margins are not auto (x={})",
    caption.bounds.x()
  );
}

