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
fn table_caption_wrapper_baseline_uses_grid_baseline() {
  let html = r#"
    <html>
      <head>
        <style>
          body { margin: 0; font-size: 20px; line-height: 20px; }
          table { display: inline-table; border-collapse: separate; border-spacing: 0; }
          td { padding: 0; border: 0; }
          caption { padding-bottom: 40px; }
        </style>
      </head>
      <body>
        <table>
          <caption>cap</caption>
          <tr><td>x</td></tr>
        </table>
      </body>
    </html>
  "#;

  let mut renderer = FastRender::new().unwrap();
  let dom = renderer.parse_html(html).unwrap();
  let tree = renderer.layout_document(&dom, 400, 200).unwrap();

  let wrapper = find_table_wrapper_with_caption(&tree.root).expect("table wrapper with caption");
  let wrapper_baseline = wrapper.baseline.expect("wrapper baseline should be set");

  let grid = wrapper
    .children
    .iter()
    .find(|child| child.style.as_ref().is_some_and(|s| is_table_like(s.display)))
    .expect("table grid child");
  let grid_baseline = grid
    .baseline
    .unwrap_or_else(|| grid.bounds.height());

  let expected = grid.bounds.y() + grid_baseline;
  assert!(
    (wrapper_baseline - expected).abs() < 0.5,
    "wrapper baseline should come from the table grid baseline (wrapper={}, expected={})",
    wrapper_baseline,
    expected
  );
}

