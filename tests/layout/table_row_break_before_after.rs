use fastrender::api::FastRender;
use fastrender::style::media::MediaType;
use fastrender::tree::fragment_tree::{FragmentContent, FragmentNode, FragmentTree};

fn pages(tree: &FragmentTree) -> Vec<&FragmentNode> {
  let mut roots = vec![&tree.root];
  roots.extend(tree.additional_fragments.iter());
  roots
}

#[derive(Debug, Clone)]
struct TextFragment {
  text: String,
}

fn collect_text_fragments(node: &FragmentNode, out: &mut Vec<TextFragment>) {
  if let FragmentContent::Text { text, .. } = &node.content {
    out.push(TextFragment {
      text: text.to_string(),
    });
  }
  for child in node.children.iter() {
    collect_text_fragments(child, out);
  }
}

fn collect_numbers(fragments: &[TextFragment]) -> Vec<usize> {
  let mut out = Vec::new();
  for frag in fragments {
    let mut digits = String::new();
    for ch in frag.text.chars() {
      if ch.is_ascii_digit() {
        digits.push(ch);
      } else if !digits.is_empty() {
        if let Ok(value) = digits.parse::<usize>() {
          out.push(value);
        }
        digits.clear();
      }
    }
    if !digits.is_empty() {
      if let Ok(value) = digits.parse::<usize>() {
        out.push(value);
      }
    }
  }
  out
}

fn assert_rows_appear_once(seen_rows: &[usize], expected_rows: usize) {
  let mut rows = seen_rows.to_vec();
  rows.sort_unstable();
  let expected: Vec<usize> = (1..=expected_rows).collect();
  assert_eq!(rows, expected);
}

#[test]
fn table_row_break_before_forces_page_break_before_each_row() {
  let row_count = 6;
  let rows: String = (1..=row_count)
    .map(|i| format!(r#"<tr><td>Row {i}</td></tr>"#))
    .collect();
  let html = format!(
    r#"
    <html>
      <head>
        <style>
          @page {{ size: 200px 120px; margin: 0; }}
          body {{ margin: 0; }}
          table {{ border-collapse: collapse; width: 100%; }}
          td {{ padding: 0; height: 30px; }}
          tr {{ break-before: page; }}
        </style>
      </head>
      <body>
        <div>Before</div>
        <table><tbody>{rows}</tbody></table>
      </body>
    </html>
  "#
  );

  let mut renderer = FastRender::new().unwrap();
  let dom = renderer.parse_html(&html).unwrap();
  let tree = renderer
    .layout_document_for_media(&dom, 200, 300, MediaType::Print)
    .unwrap();
  let page_roots = pages(&tree);

  let mut seen_rows = Vec::new();
  let mut pages_with_rows = Vec::new();
  for (page_idx, page) in page_roots.iter().enumerate() {
    let mut texts = Vec::new();
    collect_text_fragments(page, &mut texts);
    let numbers = collect_numbers(&texts);
    if !numbers.is_empty() {
      pages_with_rows.push((page_idx, numbers.clone()));
    }
    seen_rows.extend(numbers);
  }

  assert_rows_appear_once(&seen_rows, row_count);
  assert_eq!(pages_with_rows.len(), row_count);
  assert_eq!(
    pages_with_rows[0].0, 1,
    "break-before should push the first row onto a fresh page"
  );
  for (idx, (page_idx, numbers)) in pages_with_rows.iter().enumerate() {
    assert_eq!(
      numbers,
      &vec![idx + 1],
      "expected Row {} to appear alone on page {page_idx}, got {numbers:?}",
      idx + 1
    );
  }

  let mut first_page_texts = Vec::new();
  collect_text_fragments(page_roots[0], &mut first_page_texts);
  assert!(
    first_page_texts.iter().any(|t| t.text.contains("Before")),
    "expected 'Before' content on the first page"
  );
  assert!(
    collect_numbers(&first_page_texts).is_empty(),
    "expected no table rows on the first page"
  );
}

#[test]
fn table_row_break_after_forces_page_break_after_each_row() {
  let row_count = 6;
  let rows: String = (1..=row_count)
    .map(|i| format!(r#"<tr><td>Row {i}</td></tr>"#))
    .collect();
  let html = format!(
    r#"
    <html>
      <head>
        <style>
          @page {{ size: 200px 120px; margin: 0; }}
          body {{ margin: 0; }}
          table {{ border-collapse: collapse; width: 100%; }}
          td {{ padding: 0; height: 30px; }}
          tr {{ break-after: page; }}
        </style>
      </head>
      <body>
        <table><tbody>{rows}</tbody></table>
        <div>After</div>
      </body>
    </html>
  "#
  );

  let mut renderer = FastRender::new().unwrap();
  let dom = renderer.parse_html(&html).unwrap();
  let tree = renderer
    .layout_document_for_media(&dom, 200, 300, MediaType::Print)
    .unwrap();
  let page_roots = pages(&tree);

  let mut seen_rows = Vec::new();
  let mut pages_with_rows = Vec::new();
  let mut pages_with_after = Vec::new();
  for (page_idx, page) in page_roots.iter().enumerate() {
    let mut texts = Vec::new();
    collect_text_fragments(page, &mut texts);
    let numbers = collect_numbers(&texts);
    if texts.iter().any(|t| t.text.contains("After")) {
      pages_with_after.push(page_idx);
    }
    if !numbers.is_empty() {
      pages_with_rows.push((page_idx, numbers.clone()));
    }
    seen_rows.extend(numbers);
  }

  assert_rows_appear_once(&seen_rows, row_count);
  assert_eq!(pages_with_rows.len(), row_count);
  for (idx, (page_idx, numbers)) in pages_with_rows.iter().enumerate() {
    assert_eq!(
      numbers,
      &vec![idx + 1],
      "expected Row {} to appear alone on page {page_idx}, got {numbers:?}",
      idx + 1
    );
  }

  assert_eq!(
    pages_with_after,
    vec![row_count],
    "expected 'After' content to appear only on the page after the last row"
  );
}
