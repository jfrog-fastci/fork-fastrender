use fastrender::tree::fragment_tree::FragmentContent;
use fastrender::FastRender;

fn baseline_offset_for_text(fragment: &fastrender::FragmentNode, needle: &str) -> Option<f32> {
  let mut best: Option<f32> = None;
  let mut stack = vec![fragment];
  while let Some(node) = stack.pop() {
    if let FragmentContent::Text {
      text,
      baseline_offset,
      ..
    } = &node.content
    {
      if text.contains(needle) {
        best = Some(best.map_or(*baseline_offset, |b| b.min(*baseline_offset)));
      }
    }
    for child in node.children.iter().rev() {
      stack.push(child);
    }
  }
  best
}

fn layout_baseline_offset_for_aaaa(html: &str) -> f32 {
  let mut renderer = FastRender::new().expect("renderer");
  let dom = renderer.parse_html(html).expect("parse");
  let tree = renderer.layout_document(&dom, 400, 400).expect("layout");
  baseline_offset_for_text(&tree.root, "AAAA").expect("AAAA baseline_offset")
}

#[test]
fn text_emphasis_under_right_in_vertical_rl_increases_block_start_baseline_offset() {
  let base = "<html><body style=\"margin:0\"><div style=\"writing-mode: vertical-rl; font-size:40px; line-height:1; width:200px\">AAAA<br>BBBB</div></body></html>";
  let emphasis = "<html><body style=\"margin:0\"><div style=\"writing-mode: vertical-rl; font-size:40px; line-height:1; width:200px\"><span style=\"text-emphasis-style: dot; text-emphasis-position: under right;\">AAAA</span><br>BBBB</div></body></html>";

  let baseline_base = layout_baseline_offset_for_aaaa(base);
  let baseline_emphasis = layout_baseline_offset_for_aaaa(emphasis);

  assert!(
    baseline_emphasis > baseline_base + 0.5,
    "expected emphasis marks to reserve space on the block-start side in vertical-rl ({} -> {})",
    baseline_base,
    baseline_emphasis
  );
}

#[test]
fn text_emphasis_under_right_in_vertical_lr_does_not_change_block_start_baseline_offset() {
  let base = "<html><body style=\"margin:0\"><div style=\"writing-mode: vertical-lr; font-size:40px; line-height:1; width:200px\">AAAA<br>BBBB</div></body></html>";
  let emphasis = "<html><body style=\"margin:0\"><div style=\"writing-mode: vertical-lr; font-size:40px; line-height:1; width:200px\"><span style=\"text-emphasis-style: dot; text-emphasis-position: under right;\">AAAA</span><br>BBBB</div></body></html>";

  let baseline_base = layout_baseline_offset_for_aaaa(base);
  let baseline_emphasis = layout_baseline_offset_for_aaaa(emphasis);

  assert!(
    (baseline_emphasis - baseline_base).abs() < 0.5,
    "expected emphasis marks to reserve space on the block-end side in vertical-lr ({} -> {})",
    baseline_base,
    baseline_emphasis
  );
}

#[test]
fn text_emphasis_under_left_in_vertical_lr_increases_block_start_baseline_offset() {
  let base = "<html><body style=\"margin:0\"><div style=\"writing-mode: vertical-lr; font-size:40px; line-height:1; width:200px\">AAAA<br>BBBB</div></body></html>";
  let emphasis = "<html><body style=\"margin:0\"><div style=\"writing-mode: vertical-lr; font-size:40px; line-height:1; width:200px\"><span style=\"text-emphasis-style: dot; text-emphasis-position: under left;\">AAAA</span><br>BBBB</div></body></html>";

  let baseline_base = layout_baseline_offset_for_aaaa(base);
  let baseline_emphasis = layout_baseline_offset_for_aaaa(emphasis);

  assert!(
    baseline_emphasis > baseline_base + 0.5,
    "expected emphasis marks to reserve space on the block-start side in vertical-lr when positioned left ({} -> {})",
    baseline_base,
    baseline_emphasis
  );
}
