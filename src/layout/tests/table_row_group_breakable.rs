use crate::api::FastRender;
use crate::style::media::MediaType;
use crate::tree::fragment_tree::{FragmentContent, FragmentNode, FragmentTree};

fn pages(tree: &FragmentTree) -> Vec<&FragmentNode> {
  let mut roots = vec![&tree.root];
  roots.extend(tree.additional_fragments.iter());
  roots
}

#[derive(Debug, Clone)]
struct PositionedText {
  text: String,
  x: f32,
  y: f32,
}

fn collect_text_fragments(node: &FragmentNode, origin: (f32, f32), out: &mut Vec<PositionedText>) {
  let abs_x = origin.0 + node.bounds.x();
  let abs_y = origin.1 + node.bounds.y();
  if let FragmentContent::Text { text, .. } = &node.content {
    out.push(PositionedText {
      text: text.to_string(),
      x: abs_x,
      y: abs_y,
    });
  }
  for child in node.children.iter() {
    collect_text_fragments(child, (abs_x, abs_y), out);
  }
}

fn collected_text_compacted(node: &FragmentNode) -> String {
  let mut texts = Vec::new();
  collect_text_fragments(node, (0.0, 0.0), &mut texts);
  texts.sort_by(|a, b| {
    a.y
      .partial_cmp(&b.y)
      .unwrap_or(std::cmp::Ordering::Equal)
      .then(a.x.partial_cmp(&b.x).unwrap_or(std::cmp::Ordering::Equal))
  });
  let mut out = String::new();
  for t in texts {
    out.push_str(&t.text);
  }
  out.retain(|c| !c.is_whitespace());
  out
}

#[test]
fn tbody_is_breakable_by_default_in_paged_media() {
  let html = r#"
    <html>
      <head>
        <style>
          @page { size: 200px 100px; margin: 0; }
          body { margin: 0; }
          .spacer { height: 35px; }
          table { border-collapse: collapse; width: 100%; }
          th, td { padding: 0; height: 30px; line-height: 30px; }
        </style>
      </head>
      <body>
        <div class="spacer"></div>
        <table>
          <thead><tr><th>Header</th></tr></thead>
          <tbody>
            <tr><td>Row 1</td></tr>
            <tr><td>Row 2</td></tr>
          </tbody>
        </table>
      </body>
    </html>
  "#;

  let mut renderer = FastRender::new().unwrap();
  let dom = renderer.parse_html(html).unwrap();
  let tree = renderer
    .layout_document_for_media(&dom, 200, 300, MediaType::Print)
    .unwrap();
  let page_roots = pages(&tree);
  assert_eq!(
    page_roots.len(),
    2,
    "expected 2 pages; got {} (page_texts={:?})",
    page_roots.len(),
    page_roots
      .iter()
      .map(|page| collected_text_compacted(page))
      .collect::<Vec<_>>(),
  );

  let page0 = collected_text_compacted(page_roots[0]);
  let page1 = collected_text_compacted(page_roots[1]);

  assert!(
    page0.contains("Header"),
    "expected header on page 1; page_text={page0:?}"
  );
  assert!(
    page1.contains("Header"),
    "expected repeated header on page 2; page_text={page1:?}"
  );

  assert!(
    page0.contains("Row1"),
    "expected first row on page 1; page_text={page0:?}"
  );
  assert!(
    !page0.contains("Row2"),
    "did not expect second row on page 1; page_text={page0:?}"
  );
  assert!(
    page1.contains("Row2"),
    "expected second row on page 2; page_text={page1:?}"
  );
  assert!(
    !page1.contains("Row1"),
    "did not expect first row on page 2; page_text={page1:?}"
  );
}
