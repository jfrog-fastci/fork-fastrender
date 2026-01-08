use fastrender::api::FastRender;
use fastrender::style::display::Display;
use fastrender::style::types::{OutlineStyle, Overflow};
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
fn table_caption_wrapper_resets_grid_only_properties() {
  let html = r#"
    <html>
      <head>
        <style>
          body { margin: 0; }
          table {
            width: 200px;
            border-collapse: separate;
            border-spacing: 0;
            padding: 12px;
            overflow: hidden;
            outline: 4px solid red;
          }
          td { width: 10px; height: 10px; padding: 0; border: 0; }
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
  let wrapper_style = wrapper.style.as_ref().expect("wrapper style");

  // CSS 2.1 §17.4: padding/overflow/outline apply to the grid box, not the wrapper.
  assert_eq!(wrapper_style.padding_left.to_px(), 0.0);
  assert_eq!(wrapper_style.padding_right.to_px(), 0.0);
  assert_eq!(wrapper_style.padding_top.to_px(), 0.0);
  assert_eq!(wrapper_style.padding_bottom.to_px(), 0.0);
  assert_eq!(wrapper_style.overflow_x, Overflow::Visible);
  assert_eq!(wrapper_style.overflow_y, Overflow::Visible);
  assert_eq!(wrapper_style.outline_style, OutlineStyle::None);

  let grid = wrapper
    .children
    .iter()
    .find(|child| child.style.as_ref().is_some_and(|s| is_table_like(s.display)))
    .expect("table grid fragment");
  let grid_style = grid.style.as_ref().expect("grid style");
  assert!((grid_style.padding_left.to_px() - 12.0).abs() < 0.01);
  assert_eq!(grid_style.overflow_x, Overflow::Hidden);
  assert_eq!(grid_style.overflow_y, Overflow::Hidden);
  assert_eq!(grid_style.outline_style, OutlineStyle::Solid);
}

