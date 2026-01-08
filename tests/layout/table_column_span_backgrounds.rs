use fastrender::api::FastRender;
use fastrender::style::display::Display;
use fastrender::tree::fragment_tree::FragmentNode;

fn collect_fragments_with_display<'a>(
  node: &'a FragmentNode,
  display: Display,
  out: &mut Vec<&'a FragmentNode>,
) {
  if node
    .style
    .as_ref()
    .map(|style| style.display == display)
    .unwrap_or(false)
  {
    out.push(node);
  }

  for child in node.children.iter() {
    collect_fragments_with_display(child, display, out);
  }
}

fn assert_has_fragment_spanning_width(tree: &fastrender::tree::fragment_tree::FragmentTree, display: Display) {
  let mut fragments = Vec::new();
  collect_fragments_with_display(&tree.root, display, &mut fragments);
  for fragment in &tree.additional_fragments {
    collect_fragments_with_display(fragment, display, &mut fragments);
  }

  assert!(
    !fragments.is_empty(),
    "expected to find at least one fragment with display {display:?}"
  );
  let max_width = fragments
    .iter()
    .map(|fragment| fragment.bounds.width())
    .fold(0.0_f32, f32::max);
  assert!(
    (max_width - 100.0).abs() < 0.1,
    "expected a {display:?} fragment spanning ~100px, got {max_width}"
  );
}

#[test]
fn colgroup_span_applies_background_across_multiple_columns() {
  let html = r#"
    <html>
      <head>
        <style>
          table { border-collapse: separate; border-spacing: 0; table-layout: fixed; width: 100px; }
          colgroup { background: rgb(10,20,30); }
          col { width: 50px; }
          td { padding: 0; border: 0; }
        </style>
      </head>
      <body>
        <table>
          <colgroup span="2"></colgroup>
          <tr><td>A</td><td>B</td></tr>
        </table>
      </body>
    </html>
  "#;

  let mut renderer = FastRender::new().unwrap();
  let dom = renderer.parse_html(html).unwrap();
  let tree = renderer.layout_document(&dom, 200, 200).unwrap();

  assert_has_fragment_spanning_width(&tree, Display::TableColumnGroup);
}

#[test]
fn col_span_applies_background_across_multiple_columns() {
  let html = r#"
    <html>
      <head>
        <style>
          table { border-collapse: separate; border-spacing: 0; table-layout: fixed; width: 100px; }
          col { width: 50px; }
          td { padding: 0; border: 0; }
        </style>
      </head>
      <body>
        <table>
          <col span="2" style="background: rgb(10,20,30);">
          <tr><td>A</td><td>B</td></tr>
        </table>
      </body>
    </html>
  "#;

  let mut renderer = FastRender::new().unwrap();
  let dom = renderer.parse_html(html).unwrap();
  let tree = renderer.layout_document(&dom, 200, 200).unwrap();

  assert_has_fragment_spanning_width(&tree, Display::TableColumn);
}

#[test]
fn col_span_background_respects_direction_rtl_column_mapping() {
  let html = r#"
    <html>
      <head>
        <style>
          body { margin: 0; }
          table {
            border-collapse: separate;
            border-spacing: 0;
            table-layout: fixed;
            width: 150px;
            direction: rtl;
          }
          td { padding: 0; border: 0; }
        </style>
      </head>
      <body>
        <table>
          <col span="2" style="background: rgb(10,20,30);">
          <tr>
            <td style="width: 40px;">A</td>
            <td style="width: 60px;">B</td>
            <td style="width: 50px;">C</td>
          </tr>
        </table>
      </body>
    </html>
  "#;

  let mut renderer = FastRender::new().unwrap();
  let dom = renderer.parse_html(html).unwrap();
  let tree = renderer.layout_document(&dom, 200, 200).unwrap();

  let mut fragments = Vec::new();
  collect_fragments_with_display(&tree.root, Display::TableColumn, &mut fragments);
  for fragment in &tree.additional_fragments {
    collect_fragments_with_display(fragment, Display::TableColumn, &mut fragments);
  }

  let widths: Vec<f32> = fragments.iter().map(|fragment| fragment.bounds.width()).collect();
  assert!(
    widths.iter().any(|w| (*w - 100.0).abs() < 0.1),
    "expected a TableColumn fragment spanning ~100px (40px+60px) for RTL <col span>, got widths {:?}",
    widths
  );
}

#[test]
fn col_span_background_respects_direction_rtl_column_mapping_in_collapsed_border_model() {
  let html = r#"
    <html>
      <head>
        <style>
          body { margin: 0; }
          table {
            border-collapse: collapse;
            border: none;
            table-layout: fixed;
            width: 150px;
            direction: rtl;
          }
          td { padding: 0; border: 0; }
        </style>
      </head>
      <body>
        <table>
          <col span="2" style="background: rgb(10,20,30);">
          <tr>
            <td style="width: 40px;">A</td>
            <td style="width: 60px;">B</td>
            <td style="width: 50px;">C</td>
          </tr>
        </table>
      </body>
    </html>
  "#;

  let mut renderer = FastRender::new().unwrap();
  let dom = renderer.parse_html(html).unwrap();
  let tree = renderer.layout_document(&dom, 200, 200).unwrap();

  let mut fragments = Vec::new();
  collect_fragments_with_display(&tree.root, Display::TableColumn, &mut fragments);
  for fragment in &tree.additional_fragments {
    collect_fragments_with_display(fragment, Display::TableColumn, &mut fragments);
  }

  let widths: Vec<f32> = fragments.iter().map(|fragment| fragment.bounds.width()).collect();
  assert!(
    widths.iter().any(|w| (*w - 100.0).abs() < 0.1),
    "expected a TableColumn fragment spanning ~100px (40px+60px) for RTL <col span> in collapsed border model, got widths {:?}",
    widths
  );
}

#[test]
fn colgroup_span_background_respects_direction_rtl_column_mapping() {
  let html = r#"
    <html>
      <head>
        <style>
          body { margin: 0; }
          table {
            border-collapse: separate;
            border-spacing: 0;
            table-layout: fixed;
            width: 150px;
            direction: rtl;
          }
          td { padding: 0; border: 0; }
        </style>
      </head>
      <body>
        <table>
          <colgroup span="2" style="background: rgb(10,20,30);"></colgroup>
          <tr>
            <td style="width: 40px;">A</td>
            <td style="width: 60px;">B</td>
            <td style="width: 50px;">C</td>
          </tr>
        </table>
      </body>
    </html>
  "#;

  let mut renderer = FastRender::new().unwrap();
  let dom = renderer.parse_html(html).unwrap();
  let tree = renderer.layout_document(&dom, 200, 200).unwrap();

  let mut fragments = Vec::new();
  collect_fragments_with_display(&tree.root, Display::TableColumnGroup, &mut fragments);
  for fragment in &tree.additional_fragments {
    collect_fragments_with_display(fragment, Display::TableColumnGroup, &mut fragments);
  }

  let widths: Vec<f32> = fragments.iter().map(|fragment| fragment.bounds.width()).collect();
  assert!(
    widths.iter().any(|w| (*w - 100.0).abs() < 0.1),
    "expected a TableColumnGroup fragment spanning ~100px (40px+60px) for RTL <colgroup span>, got widths {:?}",
    widths
  );
}
