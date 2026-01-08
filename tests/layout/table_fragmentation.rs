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
  x: f32,
  y: f32,
}

#[derive(Debug, Clone, Copy)]
struct PositionedNumber {
  value: usize,
  pos: f32,
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

fn collect_numbers_with_positions(
  fragments: &[TextFragment],
  position: impl Fn(&TextFragment) -> f32,
) -> Vec<PositionedNumber> {
  fn flush(buf: &mut String, buf_pos: &mut Option<f32>, out: &mut Vec<PositionedNumber>) {
    if buf.is_empty() {
      return;
    }
    if let Ok(value) = buf.parse::<usize>() {
      out.push(PositionedNumber {
        value,
        pos: buf_pos.unwrap_or(0.0),
      });
    }
    buf.clear();
    *buf_pos = None;
  }

  let mut out = Vec::new();
  let mut digits = String::new();
  let mut digit_pos: Option<f32> = None;

  for frag in fragments {
    let trimmed = frag.text.trim();
    if !trimmed.is_empty() && trimmed.chars().all(|c| c.is_ascii_digit()) {
      if digit_pos.is_none() {
        digit_pos = Some(position(frag));
      }
      digits.push_str(trimmed);
      continue;
    }

    flush(&mut digits, &mut digit_pos, &mut out);

    let mut seq = String::new();
    for ch in frag.text.chars() {
      if ch.is_ascii_digit() {
        seq.push(ch);
      } else if !seq.is_empty() {
        if let Ok(value) = seq.parse::<usize>() {
          out.push(PositionedNumber {
            value,
            pos: position(frag),
          });
        }
        seq.clear();
      }
    }
    if !seq.is_empty() {
      if let Ok(value) = seq.parse::<usize>() {
        out.push(PositionedNumber {
          value,
          pos: position(frag),
        });
      }
    }
  }

  flush(&mut digits, &mut digit_pos, &mut out);
  out
}

fn collect_numbers(fragments: &[TextFragment]) -> Vec<usize> {
  collect_numbers_with_positions(fragments, |_| 0.0)
    .into_iter()
    .map(|n| n.value)
    .collect()
}

#[test]
fn table_headers_repeat_across_pages() {
  let body_rows: String = (1..=12)
    .map(|i| format!(r#"<tr><td>Row {i}</td></tr>"#))
    .collect();
  let html = format!(
    r#"
    <html>
      <head>
        <style>
          @page {{ size: 200px 120px; margin: 0; }}
          table {{ border-collapse: collapse; width: 100%; }}
          td, th {{ padding: 2px; height: 30px; }}
        </style>
      </head>
      <body>
        <table>
          <thead><tr><th>Header</th></tr></thead>
          <tbody>{body_rows}</tbody>
        </table>
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

  assert!(page_roots.len() > 1, "table should span multiple pages");

  let mut seen_rows = Vec::new();
  for (idx, page) in page_roots.iter().enumerate() {
    let mut texts = Vec::new();
    collect_text_fragments(page, &mut texts);
    assert!(
      texts.iter().any(|t| t.text.contains("Header")),
      "header should repeat on every page (missing on page {idx}; texts={:?})",
      texts.iter().map(|t| t.text.clone()).collect::<Vec<_>>()
    );
    seen_rows.extend(collect_numbers(&texts));
  }
  seen_rows.sort_unstable();
  let expected: Vec<usize> = (1..=12).collect();
  assert_eq!(seen_rows, expected);
}

#[test]
fn table_headers_repeat_in_multicol() {
  let body_rows: String = (1..=8)
    .map(|i| format!(r#"<tr><td>Row {i}</td></tr>"#))
    .collect();
  let html = format!(
    r#"
    <html>
      <head>
        <style>
          div.columns {{
            column-count: 2;
            column-gap: 20px;
            width: 260px;
          }}
          table {{ width: 100%; border-collapse: collapse; }}
          td, th {{ height: 32px; padding: 2px; }}
        </style>
      </head>
      <body>
        <div class="columns">
          <table>
            <thead><tr><th>Header</th></tr></thead>
            <tbody>{body_rows}</tbody>
          </table>
        </div>
      </body>
    </html>
  "#
  );

  let mut renderer = FastRender::new().unwrap();
  let dom = renderer.parse_html(&html).unwrap();
  let tree = renderer.layout_document(&dom, 320, 400).unwrap();

  let mut texts = Vec::new();
  collect_text_fragments(&tree.root, &mut texts);

  let header_fragments: Vec<&TextFragment> =
    texts.iter().filter(|t| t.text.contains("Header")).collect();
  let header_positions: Vec<f32> = header_fragments
    .iter()
    .map(|t| t.x)
    .collect();
  assert!(
    header_positions.len() >= 2,
    "header should repeat for each column fragment"
  );
  let min_x = header_positions
    .iter()
    .copied()
    .fold(f32::INFINITY, f32::min);
  let max_x = header_positions
    .iter()
    .copied()
    .fold(f32::NEG_INFINITY, f32::max);
  assert!(
    (max_x - min_x) > 10.0,
    "headers should appear in distinct columns"
  );

  let midpoint = (min_x + max_x) / 2.0;
  let mut first_col_rows = Vec::new();
  let mut second_col_rows = Vec::new();
  for number in collect_numbers_with_positions(&texts, |t| t.x) {
    if number.pos < midpoint {
      first_col_rows.push(number.value);
    } else {
      second_col_rows.push(number.value);
    }
  }

  first_col_rows.extend(second_col_rows.iter().copied());
  first_col_rows.sort_unstable();
  let expected: Vec<usize> = (1..=8).collect();
  assert_eq!(first_col_rows, expected);
  assert!(
    !second_col_rows.is_empty(),
    "rows should flow into the second column"
  );
}

#[test]
fn table_headers_repeat_across_pages_vertical_writing() {
  let body_rows: String = (1..=12)
    .map(|i| format!(r#"<tr><td>Row {i}</td></tr>"#))
    .collect();
  let html = format!(
    r#"
    <html>
      <head>
        <style>
          @page {{ size: 200px 120px; margin: 0; }}
          table {{ border-collapse: collapse; width: 100%; }}
          td, th {{ padding: 2px; height: 30px; writing-mode: vertical-rl; }}
        </style>
      </head>
      <body>
        <table>
          <thead><tr><th>Header</th></tr></thead>
          <tbody>{body_rows}</tbody>
        </table>
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

  assert!(page_roots.len() > 1, "table should span multiple pages");

  let mut seen_rows = Vec::new();
  for (idx, page) in page_roots.iter().enumerate() {
    let mut texts = Vec::new();
    collect_text_fragments(page, &mut texts);
    assert!(
      texts.iter().any(|t| t.text.contains("Header")),
      "header should repeat on every page (missing on page {idx}; texts={:?})",
      texts.iter().map(|t| t.text.clone()).collect::<Vec<_>>()
    );
    seen_rows.extend(collect_numbers(&texts));
  }

  seen_rows.sort_unstable();
  let expected: Vec<usize> = (1..=12).collect();
  assert_eq!(seen_rows, expected);
}

#[test]
fn table_headers_repeat_in_multicol_vertical_writing() {
  let body_rows: String = (1..=8)
    .map(|i| format!(r#"<tr><td>Row {i}</td></tr>"#))
    .collect();
  let html = format!(
    r#"
    <html>
      <head>
        <style>
          div.columns {{
            column-count: 2;
            column-gap: 20px;
            width: 260px;
          }}
          table {{ width: 100%; border-collapse: collapse; }}
          td, th {{ height: 32px; padding: 2px; writing-mode: vertical-rl; }}
        </style>
      </head>
      <body>
        <div class="columns">
          <table>
            <thead><tr><th>Header</th></tr></thead>
            <tbody>{body_rows}</tbody>
          </table>
        </div>
      </body>
    </html>
  "#
  );

  let mut renderer = FastRender::new().unwrap();
  let dom = renderer.parse_html(&html).unwrap();
  let tree = renderer.layout_document(&dom, 320, 400).unwrap();

  let mut texts = Vec::new();
  collect_text_fragments(&tree.root, &mut texts);

  let header_fragments: Vec<&TextFragment> =
    texts.iter().filter(|t| t.text.contains("Header")).collect();
  let header_physical: Vec<(f32, f32)> = header_fragments.iter().map(|t| (t.x, t.y)).collect();
  let (min_x, max_x) = header_fragments.iter().fold((f32::INFINITY, f32::NEG_INFINITY), |acc, t| {
    (acc.0.min(t.x), acc.1.max(t.x))
  });
  let (min_y, max_y) = header_fragments.iter().fold((f32::INFINITY, f32::NEG_INFINITY), |acc, t| {
    (acc.0.min(t.y), acc.1.max(t.y))
  });
  let x_spread = max_x - min_x;
  let y_spread = max_y - min_y;
  let column_axis: fn(&TextFragment) -> f32 = if x_spread >= y_spread {
    |t: &TextFragment| t.x
  } else {
    |t: &TextFragment| t.y
  };
  let header_positions: Vec<f32> = header_fragments
    .iter()
    .map(|t| column_axis(t))
    .collect();
  assert!(
    header_positions.len() >= 2,
    "header should repeat for each column fragment"
  );
  let min_inline = header_positions
    .iter()
    .copied()
    .fold(f32::INFINITY, f32::min);
  let max_inline = header_positions
    .iter()
    .copied()
    .fold(f32::NEG_INFINITY, f32::max);
  assert!(
    (max_inline - min_inline) > 10.0,
    "headers should appear in distinct columns (min={min_inline}, max={max_inline}, positions={header_positions:?}, physical={header_physical:?})"
  );

  let midpoint = (min_inline + max_inline) / 2.0;
  let mut first_col_rows = Vec::new();
  let mut second_col_rows = Vec::new();
  for number in collect_numbers_with_positions(&texts, column_axis) {
    if number.pos < midpoint {
      first_col_rows.push(number.value);
    } else {
      second_col_rows.push(number.value);
    }
  }

  first_col_rows.extend(second_col_rows.iter().copied());
  first_col_rows.sort_unstable();
  let expected: Vec<usize> = (1..=8).collect();
  assert_eq!(first_col_rows, expected);
  assert!(
    !second_col_rows.is_empty(),
    "rows should flow into the second column"
  );
}
