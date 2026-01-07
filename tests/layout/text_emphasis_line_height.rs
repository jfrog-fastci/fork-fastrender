use fastrender::tree::fragment_tree::FragmentContent;
use fastrender::FastRender;

fn baseline_for_text(fragment: &fastrender::FragmentNode, needle: &str) -> Option<f32> {
  let mut best: Option<f32> = None;
  let mut stack = vec![(fragment, 0.0f32)];
  while let Some((node, offset_y)) = stack.pop() {
    let current_y = offset_y + node.bounds.y();
    if let FragmentContent::Text {
      text,
      baseline_offset,
      ..
    } = &node.content
    {
      if text.contains(needle) {
        let baseline = current_y + *baseline_offset;
        best = Some(best.map_or(baseline, |b| b.min(baseline)));
      }
    }
    for child in node.children.iter().rev() {
      stack.push((child, current_y));
    }
  }
  best
}

fn layout_baseline_for_bbbb(html: &str) -> f32 {
  let mut renderer = FastRender::new().expect("renderer");
  let dom = renderer.parse_html(html).expect("parse");
  let tree = renderer.layout_document(&dom, 400, 400).expect("layout");
  baseline_for_text(&tree.root, "BBBB").expect("BBBB baseline")
}

#[test]
fn text_emphasis_over_increases_line_height() {
  let base = "<html><body><div style=\"font-size:40px; line-height:1; width:200px\">AAAA<br>BBBB</div></body></html>";
  let emphasis = "<html><body><div style=\"font-size:40px; line-height:1; width:200px\"><span style=\"text-emphasis-style: dot; text-emphasis-position: over;\">AAAA</span><br>BBBB</div></body></html>";

  let baseline_base = layout_baseline_for_bbbb(base);
  let baseline_emphasis = layout_baseline_for_bbbb(emphasis);

  assert!(
    baseline_emphasis > baseline_base + 0.5,
    "expected emphasized first line to push second line down ({} -> {})",
    baseline_base,
    baseline_emphasis
  );
}

#[test]
fn text_emphasis_under_increases_line_height() {
  let base = "<html><body><div style=\"font-size:40px; line-height:1; width:200px\">AAAA<br>BBBB</div></body></html>";
  let emphasis = "<html><body><div style=\"font-size:40px; line-height:1; width:200px\"><span style=\"text-emphasis-style: dot; text-emphasis-position: under;\">AAAA</span><br>BBBB</div></body></html>";

  let baseline_base = layout_baseline_for_bbbb(base);
  let baseline_emphasis = layout_baseline_for_bbbb(emphasis);

  assert!(
    baseline_emphasis > baseline_base + 0.5,
    "expected emphasized first line to push second line down ({} -> {})",
    baseline_base,
    baseline_emphasis
  );
}

