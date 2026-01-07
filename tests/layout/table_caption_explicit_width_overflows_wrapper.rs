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
fn table_caption_explicit_width_overflows_wrapper() {
  let html = r#"
    <html>
      <head>
        <style>
          body { margin: 0; }
          table { display: inline-table; border-collapse: separate; border-spacing: 0; }
          table, caption, td { padding: 0; border: 0; }
          td { width: 40px; height: 10px; }
          caption { width: 200px; }
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
    .find(|child| child.style.as_ref().is_some_and(|s| is_table_like(s.display)))
    .expect("table grid fragment");

  assert!(
    (wrapper.bounds.width() - grid.bounds.width()).abs() < 0.1,
    "wrapper width should equal grid width (wrapper={}, grid={})",
    wrapper.bounds.width(),
    grid.bounds.width()
  );
  assert!(
    wrapper.bounds.width() < 100.0,
    "table should remain near its cell-derived width (got {})",
    wrapper.bounds.width()
  );
  assert!(
    (caption.bounds.width() - 200.0).abs() < 0.1,
    "caption should respect its explicit width even when it overflows (got {})",
    caption.bounds.width()
  );
  assert!(
    caption.bounds.width() > wrapper.bounds.width() + 50.0,
    "caption should be wider than the wrapper (caption={}, wrapper={})",
    caption.bounds.width(),
    wrapper.bounds.width()
  );
  assert!(
    caption.bounds.x().abs() < 0.1,
    "caption should start at the wrapper's left edge when margins are not auto (x={})",
    caption.bounds.x()
  );
}

