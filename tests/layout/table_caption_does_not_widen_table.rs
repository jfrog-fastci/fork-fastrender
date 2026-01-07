use fastrender::api::FastRender;
use fastrender::style::display::Display;
use fastrender::tree::fragment_tree::FragmentNode;

fn is_table_wrapper_display(display: Display) -> bool {
  matches!(display, Display::Table | Display::InlineTable)
}

fn find_table_wrapper_with_caption<'a>(node: &'a FragmentNode) -> Option<&'a FragmentNode> {
  if node
    .style
    .as_ref()
    .is_some_and(|style| is_table_wrapper_display(style.display))
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
fn table_caption_does_not_widen_table() {
  let html = r#"
    <html>
      <head>
        <style>
          body { margin: 0; }
          table { display: inline-table; border-collapse: separate; border-spacing: 0; }
          table, caption, td { padding: 0; border: 0; }
          td { width: 40px; height: 10px; }
          caption { white-space: nowrap; }
        </style>
      </head>
      <body>
        <table>
          <caption>THIS_IS_A_VERY_LONG_UNBREAKABLE_CAPTION_THAT_SHOULD_NOT_AFFECT_TABLE_WIDTH_THIS_IS_A_VERY_LONG_UNBREAKABLE_CAPTION_THAT_SHOULD_NOT_AFFECT_TABLE_WIDTH</caption>
          <tr><td></td></tr>
        </table>
      </body>
    </html>
  "#;

  let mut renderer = FastRender::new().unwrap();
  let dom = renderer.parse_html(html).unwrap();
  let tree = renderer.layout_document(&dom, 800, 200).unwrap();

  let wrapper = find_table_wrapper_with_caption(&tree.root).expect("table wrapper with caption");
  let caption = wrapper
    .children
    .iter()
    .find(|child| matches!(child.style.as_ref().map(|s| s.display), Some(Display::TableCaption)))
    .expect("caption fragment");
  let grid = wrapper
    .children
    .iter()
    .find(|child| {
      child
        .style
        .as_ref()
        .is_some_and(|style| is_table_wrapper_display(style.display))
    })
    .expect("table grid fragment");

  assert!(
    (wrapper.bounds.width() - grid.bounds.width()).abs() < 0.1,
    "wrapper width should equal the table grid width (wrapper={}, grid={})",
    wrapper.bounds.width(),
    grid.bounds.width()
  );
  assert!(
    wrapper.bounds.width() < 100.0,
    "table should remain near its cell-derived width (got {})",
    wrapper.bounds.width()
  );
  assert!(
    (caption.bounds.width() - wrapper.bounds.width()).abs() < 0.1,
    "caption with width:auto should fill the wrapper width (caption={}, wrapper={})",
    caption.bounds.width(),
    wrapper.bounds.width()
  );
}
