use crate::api::FastRender;
use crate::style::media::MediaType;
use crate::tree::fragment_tree::{FragmentContent, FragmentNode, FragmentTree};

fn pages(tree: &FragmentTree) -> Vec<&FragmentNode> {
  let mut roots = vec![&tree.root];
  roots.extend(tree.additional_fragments.iter());
  roots
}

#[derive(Debug, Clone)]
struct TextFragment {
  text: String,
  x: f32,
  y: f32,
}

fn collect_text_fragments(node: &FragmentNode, out: &mut Vec<TextFragment>) {
  collect_text_fragments_with_origin(node, (0.0, 0.0), out);
}

fn collect_text_fragments_with_origin(
  node: &FragmentNode,
  origin: (f32, f32),
  out: &mut Vec<TextFragment>,
) {
  let abs_x = origin.0 + node.bounds.x();
  let abs_y = origin.1 + node.bounds.y();
  if let FragmentContent::Text { text, .. } = &node.content {
    out.push(TextFragment {
      text: text.to_string(),
      x: abs_x,
      y: abs_y,
    });
  }
  for child in node.children.iter() {
    collect_text_fragments_with_origin(child, (abs_x, abs_y), out);
  }
}

fn collect_numbers(fragments: &[TextFragment]) -> Vec<usize> {
  let mut numbers = Vec::new();
  for frag in fragments {
    let mut seq = String::new();
    for ch in frag.text.chars() {
      if ch.is_ascii_digit() {
        seq.push(ch);
      } else if !seq.is_empty() {
        if let Ok(value) = seq.parse::<usize>() {
          numbers.push(value);
        }
        seq.clear();
      }
    }
    if !seq.is_empty() {
      if let Ok(value) = seq.parse::<usize>() {
        numbers.push(value);
      }
    }
  }
  numbers
}

#[test]
fn tbody_break_inside_avoid_moves_group_to_next_page() {
  let html = r#"
    <html>
      <head>
        <style>
          @page { size: 200px 100px; margin: 0; }
          body { margin: 0; }
          .spacer { height: 40px; }
          table { border-collapse: collapse; width: 100%; }
          td { padding: 0; height: 20px; }
        </style>
      </head>
      <body>
        <div class="spacer"></div>
        <table>
          <tbody style="break-inside: avoid">
            <tr><td>Row 1</td></tr>
            <tr><td>Row 2</td></tr>
            <tr><td>Row 3</td></tr>
            <tr><td>Row 4</td></tr>
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
  assert!(page_roots.len() > 1, "expected content to paginate");

  let expected_rows = vec![1usize, 2, 3, 4];
  let mut per_page = Vec::new();
  for page in page_roots.iter() {
    let mut texts = Vec::new();
    collect_text_fragments(page, &mut texts);
    let mut numbers = collect_numbers(&texts);
    numbers.sort_unstable();
    per_page.push(numbers);
  }

  assert!(
    per_page.first().is_some_and(|nums| nums.is_empty()),
    "tbody should move to the next page due to break-inside: avoid (page0 rows={:?}, all_pages={per_page:?})",
    per_page.first()
  );
  assert!(
    per_page.get(1).is_some_and(|nums| *nums == expected_rows),
    "expected all tbody rows on page 1 (all_pages={per_page:?})"
  );
  for (idx, nums) in per_page.iter().enumerate().skip(2) {
    assert!(
      nums.is_empty(),
      "expected no tbody rows beyond page 1 (page {idx} has rows {nums:?}; all_pages={per_page:?})"
    );
  }
}

#[test]
fn tbody_break_inside_avoid_moves_group_to_next_column() {
  let html = r#"
    <html>
      <head>
        <style>
          body { margin: 0; }
          div.columns { column-count: 2; column-gap: 0; width: 200px; height: 100px; column-fill: auto; }
          div.spacer { height: 40px; }
          table { border-collapse: collapse; width: 100%; }
          td { padding: 0; height: 20px; }
        </style>
      </head>
      <body>
        <div class="columns">
          <div class="spacer">Before</div>
          <table>
            <tbody style="break-inside: avoid">
              <tr><td>Row 1</td></tr>
              <tr><td>Row 2</td></tr>
              <tr><td>Row 3</td></tr>
              <tr><td>Row 4</td></tr>
            </tbody>
          </table>
        </div>
      </body>
    </html>
  "#;

  let mut renderer = FastRender::new().unwrap();
  let dom = renderer.parse_html(html).unwrap();
  let tree = renderer.layout_document(&dom, 400, 200).unwrap();

  let mut texts = Vec::new();
  collect_text_fragments(&tree.root, &mut texts);

  let before_x = texts
    .iter()
    .filter(|t| t.text.contains("Before"))
    .map(|t| t.x)
    .fold(f32::INFINITY, f32::min);
  assert!(
    before_x.is_finite(),
    "expected to find 'Before' text fragment"
  );

  let row_fragments: Vec<TextFragment> = texts
    .into_iter()
    .filter(|t| t.text.contains("Row"))
    .collect();
  assert!(
    !row_fragments.is_empty(),
    "expected to find row text fragments"
  );

  let row_positions: Vec<(String, f32, f32)> = row_fragments
    .iter()
    .map(|t| (t.text.clone(), t.x, t.y))
    .collect();
  let min_row_x = row_fragments
    .iter()
    .map(|t| t.x)
    .fold(f32::INFINITY, f32::min);
  assert!(
    min_row_x > before_x + 10.0,
    "tbody should move to the next column due to break-inside: avoid (before_x={before_x}, min_row_x={min_row_x}, row_positions={row_positions:?})"
  );

  let mut numbers = collect_numbers(&row_fragments);
  numbers.sort_unstable();
  numbers.dedup();
  assert_eq!(numbers, vec![1usize, 2, 3, 4]);
}
