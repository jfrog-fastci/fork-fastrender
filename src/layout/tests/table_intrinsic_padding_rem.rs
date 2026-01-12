use crate::api::FastRender;
use crate::style::display::Display;
use crate::tree::fragment_tree::{FragmentContent, FragmentNode};

fn collect_text(node: &FragmentNode, out: &mut String) {
  if let FragmentContent::Text { text, .. } = &node.content {
    out.push_str(text);
  }
  for child in node.children.iter() {
    collect_text(child, out);
  }
}

fn find_table_cell_containing<'a>(
  node: &'a FragmentNode,
  needle: &str,
) -> Option<&'a FragmentNode> {
  if matches!(
    node.style.as_ref().map(|s| s.display),
    Some(Display::TableCell)
  ) {
    let mut text = String::new();
    collect_text(node, &mut text);
    if text.contains(needle) {
      return Some(node);
    }
  }
  node
    .children
    .iter()
    .find_map(|child| find_table_cell_containing(child, needle))
}

fn count_line_fragments(node: &FragmentNode) -> usize {
  let mut count = 0usize;
  let mut stack = vec![node];
  while let Some(node) = stack.pop() {
    if matches!(node.content, FragmentContent::Line { .. }) {
      count += 1;
    }
    for child in node.children.iter() {
      stack.push(child);
    }
  }
  count
}

#[test]
fn table_intrinsic_sizing_accounts_for_rem_padding() {
  // Regression test: table auto-layout must account for font-relative (e.g. `rem`) cell padding when
  // computing intrinsic column widths. Otherwise, the column distribution can allocate a column
  // width that fits the cell's text but not its padding, causing emergency wraps via
  // `overflow-wrap: break-word`.
  let html = r#"
    <html>
      <head>
        <style>
          html { font-size: 16px; }
          body { margin: 0; }
          table { border-collapse: collapse; width: 496px; }
          th, td { border: 1px solid rgb(195, 199, 203); padding: 0.75rem; }
          /* Allow emergency wrapping for overflowing long words, like MDN does. */
          th { overflow-wrap: break-word; }
        </style>
      </head>
      <body>
        <table>
          <tbody>
            <tr>
              <th>Prerequisites:</th>
              <td>A basic understanding of HTML.</td>
            </tr>
          </tbody>
        </table>
      </body>
    </html>
  "#;

  let mut renderer = FastRender::new().unwrap();
  let dom = renderer.parse_html(html).unwrap();
  let tree = renderer.layout_document(&dom, 600, 200).unwrap();

  let th = find_table_cell_containing(&tree.root, "Prerequisites")
    .expect("expected to find the prerequisites table cell");
  let line_count = count_line_fragments(th);
  assert_eq!(
    line_count, 1,
    "expected prerequisites cell to lay out on a single line (got {line_count})"
  );
}
