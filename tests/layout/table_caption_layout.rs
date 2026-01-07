use fastrender::api::FastRender;
use fastrender::style::display::Display;
use fastrender::tree::fragment_tree::FragmentNode;

fn fragment_has_display(fragment: &FragmentNode, display: Display) -> bool {
  fragment
    .style
    .as_ref()
    .is_some_and(|style| style.display == display)
}

fn find_table_wrapper<'a>(fragment: &'a FragmentNode) -> Option<&'a FragmentNode> {
  if fragment_has_display(fragment, Display::Table) || fragment_has_display(fragment, Display::InlineTable) {
    if fragment
      .children
      .iter()
      .any(|child| fragment_has_display(child, Display::TableCaption))
    {
      return Some(fragment);
    }
  }
  for child in fragment.children.iter() {
    if let Some(found) = find_table_wrapper(child) {
      return Some(found);
    }
  }
  None
}

fn find_direct_child_with_display<'a>(
  parent: &'a FragmentNode,
  display: Display,
) -> Option<&'a FragmentNode> {
  parent
    .children
    .iter()
    .find(|child| fragment_has_display(child, display))
}

#[test]
fn caption_does_not_widen_table_to_max_content() {
  let html = r#"
    <html>
      <head>
        <style>
          body { margin: 0; }
          table { border-collapse: separate; border-spacing: 0; }
          caption { white-space: normal; }
        </style>
      </head>
      <body>
        <table>
          <caption>
            one two three four five six seven eight nine ten eleven twelve thirteen fourteen fifteen
          </caption>
          <tr><td>x</td></tr>
        </table>
      </body>
    </html>
  "#;

  let viewport_width = 200u32;
  let mut renderer = FastRender::new().expect("renderer");
  let dom = renderer.parse_html(html).expect("parse");
  let tree = renderer
    .layout_document(&dom, viewport_width, 200)
    .expect("layout");

  let wrapper = find_table_wrapper(&tree.root).expect("table wrapper fragment");
  let caption = find_direct_child_with_display(wrapper, Display::TableCaption).expect("caption");
  let table_grid = wrapper
    .children
    .iter()
    .find(|child| fragment_has_display(child, Display::Table) || fragment_has_display(child, Display::InlineTable))
    .expect("table grid fragment");

  assert!(
    table_grid.bounds.width() <= viewport_width as f32 + 0.5,
    "table grid width should not expand to caption max-content (w={:.2}, viewport={})",
    table_grid.bounds.width(),
    viewport_width
  );
  assert!(
    caption.bounds.height() > 30.0,
    "caption should wrap to multiple lines (h={:.2})",
    caption.bounds.height()
  );
}

#[test]
fn caption_with_explicit_width_and_auto_margins_is_centered() {
  let html = r#"
    <html>
      <head>
        <style>
          body { margin: 0; }
          table { width: 200px; border-collapse: separate; border-spacing: 0; }
          caption { width: 80px; margin-left: auto; margin-right: auto; }
        </style>
      </head>
      <body>
        <table>
          <caption>caption</caption>
          <tr><td>x</td></tr>
        </table>
      </body>
    </html>
  "#;

  let mut renderer = FastRender::new().expect("renderer");
  let dom = renderer.parse_html(html).expect("parse");
  let tree = renderer.layout_document(&dom, 240, 200).expect("layout");

  let wrapper = find_table_wrapper(&tree.root).expect("table wrapper fragment");
  let caption = find_direct_child_with_display(wrapper, Display::TableCaption).expect("caption");
  let wrapper_width = wrapper.bounds.width();
  let caption_width = caption.bounds.width();
  let expected_x = (wrapper_width - caption_width) * 0.5;

  assert!(
    (caption.bounds.x() - expected_x).abs() < 0.75,
    "expected centered caption x≈{expected_x:.2} (got x={:.2}, wrapper_w={wrapper_width:.2}, caption_w={caption_width:.2})",
    caption.bounds.x()
  );
}

#[test]
fn caption_vertical_margins_affect_table_stacking() {
  let html = r#"
    <html>
      <head>
        <style>
          body { margin: 0; }
          table { width: 200px; border-collapse: separate; border-spacing: 0; }
          caption { margin-bottom: 10px; }
        </style>
      </head>
      <body>
        <table>
          <caption>caption</caption>
          <tr><td>x</td></tr>
        </table>
      </body>
    </html>
  "#;

  let mut renderer = FastRender::new().expect("renderer");
  let dom = renderer.parse_html(html).expect("parse");
  let tree = renderer.layout_document(&dom, 240, 200).expect("layout");

  let wrapper = find_table_wrapper(&tree.root).expect("table wrapper fragment");
  let caption = find_direct_child_with_display(wrapper, Display::TableCaption).expect("caption");
  let table_grid = wrapper
    .children
    .iter()
    .find(|child| fragment_has_display(child, Display::Table) || fragment_has_display(child, Display::InlineTable))
    .expect("table grid fragment");

  let expected_table_y = caption.bounds.y() + caption.bounds.height() + 10.0;
  assert!(
    (table_grid.bounds.y() - expected_table_y).abs() < 0.75,
    "expected table y≈{expected_table_y:.2} (got y={:.2}, caption_y={:.2}, caption_h={:.2})",
    table_grid.bounds.y(),
    caption.bounds.y(),
    caption.bounds.height()
  );
}

