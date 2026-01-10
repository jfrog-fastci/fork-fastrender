use fastrender::style::types::VerticalAlign as CssVerticalAlign;
use fastrender::tree::fragment_tree::{FragmentContent, FragmentNode};
use fastrender::{FastRender, FontConfig};

fn contains_text(fragment: &FragmentNode, needle: &str) -> bool {
  let mut stack = vec![fragment];
  while let Some(node) = stack.pop() {
    if let FragmentContent::Text { text, .. } = &node.content {
      if text.contains(needle) {
        return true;
      }
    }
    for child in node.children.iter().rev() {
      stack.push(child);
    }
  }
  false
}

fn find_line_with_text_abs_y<'a>(
  node: &'a FragmentNode,
  needle: &str,
  parent_y: f32,
) -> Option<(f32, &'a FragmentNode)> {
  let abs_y = parent_y + node.bounds.y();
  if matches!(node.content, FragmentContent::Line { .. }) && contains_text(node, needle) {
    return Some((abs_y, node));
  }
  for child in node.children.iter() {
    if let Some(found) = find_line_with_text_abs_y(child, needle, abs_y) {
      return Some(found);
    }
  }
  None
}

fn find_replaced_abs_y<'a>(
  node: &'a FragmentNode,
  parent_y: f32,
) -> Option<(f32, &'a FragmentNode)> {
  let abs_y = parent_y + node.bounds.y();
  if matches!(node.content, FragmentContent::Replaced { .. }) {
    return Some((abs_y, node));
  }
  for child in node.children.iter() {
    if let Some(found) = find_replaced_abs_y(child, abs_y) {
      return Some(found);
    }
  }
  None
}

#[test]
fn vertical_align_middle_affects_inline_replaced_line_box_metrics() {
  let mut renderer = FastRender::builder()
    .font_sources(FontConfig::bundled_only())
    .build()
    .expect("renderer");

  let data_png = "data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMB/6XWcJAAAAAASUVORK5CYII=";

  let html = format!(
    r#"
    <html>
      <head>
        <style>
          .v-mid {{ vertical-align: middle; }}
        </style>
      </head>
      <body style="margin:0; font-family:sans-serif; font-size:20px; line-height:20px">
        <div><img src="{data_png}" style="width:80px; height:80px">BASE</div>
        <div><img class="v-mid" src="{data_png}" style="width:80px; height:80px">MID</div>
      </body>
    </html>
  "#,
  );

  let dom = renderer.parse_html(&html).expect("parse");
  let tree = renderer.layout_document(&dom, 800, 200).expect("layout");

  let (base_line_y, base_line) =
    find_line_with_text_abs_y(&tree.root, "BASE", 0.0).expect("BASE line fragment");
  let (mid_line_y, mid_line) =
    find_line_with_text_abs_y(&tree.root, "MID", 0.0).expect("MID line fragment");

  let FragmentContent::Line {
    baseline: base_baseline,
  } = base_line.content
  else {
    panic!("expected BASE fragment to be a line");
  };
  let FragmentContent::Line {
    baseline: mid_baseline,
  } = mid_line.content
  else {
    panic!("expected MID fragment to be a line");
  };

  let (base_img_y, base_img) = base_line
    .children
    .iter()
    .find_map(|child| find_replaced_abs_y(child, base_line_y))
    .expect("BASE line should contain a replaced fragment");
  let (mid_img_y, mid_img) = mid_line
    .children
    .iter()
    .find_map(|child| find_replaced_abs_y(child, mid_line_y))
    .expect("MID line should contain a replaced fragment");

  let Some(base_img_style) = base_img.style.as_deref() else {
    panic!("expected BASE image fragment to carry style");
  };
  let Some(mid_img_style) = mid_img.style.as_deref() else {
    panic!("expected MID image fragment to carry style");
  };

  assert!(
    matches!(base_img_style.vertical_align, CssVerticalAlign::Baseline),
    "expected BASE image to keep default vertical-align: baseline, got {:?}",
    base_img_style.vertical_align
  );
  assert!(
    matches!(mid_img_style.vertical_align, CssVerticalAlign::Middle),
    "expected `.v-mid` class to set vertical-align: middle, got {:?}",
    mid_img_style.vertical_align
  );

  let base_line_height = base_line.bounds.height();
  let mid_line_height = mid_line.bounds.height();
  assert!(
    base_line_height > mid_line_height + 0.5,
    "expected vertical-align: middle to reduce line box height (base={base_line_height}, mid={mid_line_height})"
  );

  assert!(
    base_baseline > mid_baseline + 0.5,
    "expected vertical-align: middle to reduce the line baseline position (base={base_baseline}, mid={mid_baseline})"
  );

  let base_baseline_abs = base_line_y + base_baseline;
  let mid_baseline_abs = mid_line_y + mid_baseline;

  let base_img_bottom_abs = base_img_y + base_img.bounds.height();
  assert!(
    (base_img_bottom_abs - base_baseline_abs).abs() < 0.5,
    "expected baseline-aligned replaced element to have its bottom edge on the line baseline (img_bottom={base_img_bottom_abs}, baseline={base_baseline_abs})"
  );

  let mid_img_bottom_abs = mid_img_y + mid_img.bounds.height();
  assert!(
    mid_img_bottom_abs > mid_baseline_abs + 0.5,
    "expected vertical-align: middle to move replaced element baseline below the line baseline (img_bottom={mid_img_bottom_abs}, baseline={mid_baseline_abs})"
  );
}

#[test]
fn vertical_align_middle_affects_inline_replaced_inside_inline_box_line_metrics() {
  let mut renderer = FastRender::builder()
    .font_sources(FontConfig::bundled_only())
    .build()
    .expect("renderer");

  let data_png = "data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMB/6XWcJAAAAAASUVORK5CYII=";

  let html = format!(
    r#"
    <html>
      <head>
        <style>
          .v-mid {{ vertical-align: middle; }}
        </style>
      </head>
      <body style="margin:0; font-family:sans-serif; font-size:20px; line-height:20px">
        <div><a href="x"><img src="{data_png}" style="width:80px; height:80px">BASE</a></div>
        <div><a href="x"><img class="v-mid" src="{data_png}" style="width:80px; height:80px">MID</a></div>
      </body>
    </html>
  "#,
  );

  let dom = renderer.parse_html(&html).expect("parse");
  let tree = renderer.layout_document(&dom, 800, 200).expect("layout");

  let (_base_line_y, base_line) =
    find_line_with_text_abs_y(&tree.root, "BASE", 0.0).expect("BASE line fragment");
  let (_mid_line_y, mid_line) =
    find_line_with_text_abs_y(&tree.root, "MID", 0.0).expect("MID line fragment");

  let base_line_height = base_line.bounds.height();
  let mid_line_height = mid_line.bounds.height();
  assert!(
    base_line_height > mid_line_height + 0.5,
    "expected vertical-align: middle to reduce line box height even when the replaced element is inside an inline box (base={base_line_height}, mid={mid_line_height})"
  );
}
