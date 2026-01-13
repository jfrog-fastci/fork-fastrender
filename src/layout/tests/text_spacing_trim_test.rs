use crate::tree::fragment_tree::{FragmentContent, FragmentNode};
use crate::{FastRender, FontConfig};

fn find_first_block_with_line_children<'a>(node: &'a FragmentNode) -> Option<&'a FragmentNode> {
  if matches!(node.content, FragmentContent::Block { .. })
    && node
      .children
      .iter()
      .any(|child| matches!(child.content, FragmentContent::Line { .. }))
  {
    return Some(node);
  }

  for child in node.children.iter() {
    if let Some(found) = find_first_block_with_line_children(child) {
      return Some(found);
    }
  }
  None
}

fn line_children(block: &FragmentNode) -> Vec<FragmentNode> {
  block
    .children
    .iter()
    .filter(|child| matches!(child.content, FragmentContent::Line { .. }))
    .cloned()
    .collect()
}

fn first_text_x_in_line(line: &FragmentNode) -> Option<f32> {
  fn walk(node: &FragmentNode, offset_x: f32) -> Option<f32> {
    let offset_x = offset_x + node.bounds.x();
    if matches!(node.content, FragmentContent::Text { .. }) {
      return Some(offset_x);
    }
    for child in node.children.iter() {
      if let Some(found) = walk(child, offset_x) {
        return Some(found);
      }
    }
    None
  }

  for child in line.children.iter() {
    if let Some(found) = walk(child, 0.0) {
      return Some(found);
    }
  }
  None
}

fn layout_lines(text_spacing_trim: &str, text_align: &str) -> Vec<FragmentNode> {
  let html = format!(
    r#"
      <style>
        body {{ margin: 0; }}
        .box {{
          width: 60px;
          font-family: 'DejaVu Sans', sans-serif;
          font-size: 24px;
          line-height: 1;
          word-break: break-all;
          text-align: {text_align};
          text-spacing-trim: {text_spacing_trim};
        }}
      </style>
      <div class="box">「HELLOWORLD」</div>
    "#
  );

  let mut renderer = FastRender::builder()
    .font_sources(FontConfig::bundled_only())
    .build()
    .expect("build renderer");

  let dom = renderer.parse_html(&html).expect("parse HTML");
  let fragments = renderer
    .layout_document(&dom, 400, 200)
    .expect("layout document");

  let block = find_first_block_with_line_children(&fragments.root)
    .expect("expected a block fragment with line children");
  line_children(block)
}

#[test]
fn text_spacing_trim_hangs_fullwidth_punctuation() {
  // Compare default behavior vs `trim-both`.
  let space_all_left = layout_lines("space-all", "left");
  let trim_both_left = layout_lines("trim-both", "left");

  let space_first_x = first_text_x_in_line(&space_all_left[0]).expect("first text x (space-all)");
  let trim_first_x = first_text_x_in_line(&trim_both_left[0]).expect("first text x (trim-both)");

  assert!(
    trim_first_x < space_first_x - 0.1,
    "expected trim-both to shift the leading opening punctuation left: space-all x={space_first_x:.3} trim-both x={trim_first_x:.3}"
  );
  assert!(
    trim_first_x < 0.0,
    "expected leading punctuation to hang into the start edge (negative x), got {trim_first_x:.3}"
  );

  // Use `text-align: right` so the trimmed closing punctuation hangs past the line end.
  let space_all_right = layout_lines("space-all", "right");
  let trim_both_right = layout_lines("trim-both", "right");

  let last_space_line = space_all_right.last().expect("space-all last line");
  let last_trim_line = trim_both_right.last().expect("trim-both last line");

  let space_bbox = last_space_line.bounding_box();
  let trim_bbox = last_trim_line.bounding_box();

  let space_end = last_space_line.bounds.max_x();
  let trim_end = last_trim_line.bounds.max_x();

  assert!(
    space_bbox.max_x() <= space_end + 0.05,
    "expected space-all to keep punctuation within the line bounds (bbox_end={:.3}, line_end={:.3})",
    space_bbox.max_x(),
    space_end
  );
  assert!(
    trim_bbox.max_x() > trim_end + 0.05,
    "expected trim-both to hang closing punctuation past the line end (bbox_end={:.3}, line_end={:.3})",
    trim_bbox.max_x(),
    trim_end
  );
}

