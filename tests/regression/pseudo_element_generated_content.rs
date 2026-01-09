use fastrender::api::FastRender;
use fastrender::tree::fragment_tree::FragmentContent;
use tiny_skia::Pixmap;

fn pixel(pixmap: &Pixmap, x: u32, y: u32) -> (u8, u8, u8, u8) {
  let p = pixmap.pixel(x, y).unwrap();
  (p.red(), p.green(), p.blue(), p.alpha())
}

fn find_exact_color_bbox(
  pixmap: &Pixmap,
  target: (u8, u8, u8, u8),
) -> Option<(u32, u32, u32, u32)> {
  let mut min_x = u32::MAX;
  let mut min_y = u32::MAX;
  let mut max_x = 0u32;
  let mut max_y = 0u32;
  let mut found = false;

  for y in 0..pixmap.height() {
    for x in 0..pixmap.width() {
      if pixel(pixmap, x, y) == target {
        found = true;
        min_x = min_x.min(x);
        min_y = min_y.min(y);
        max_x = max_x.max(x);
        max_y = max_y.max(y);
      }
    }
  }

  found.then_some((min_x, min_y, max_x, max_y))
}

#[test]
fn pseudo_element_generated_text_does_not_inherit_layout_properties() {
  let html = r#"<!doctype html>
<style>
  html, body { margin: 0; padding: 0; background: #fff; }
  .ad { position: relative; width: 200px; height: 50px; }
  .ad::before {
    content: "ADVERTISEMENT";
    position: absolute;
    top: 0;
    left: 0;
    width: 200px;
    height: 30px;
    display: flex;
    justify-content: center;
    align-items: center;
    background: #242424;
    color: #fff;
    font-size: 12px;
  }
</style>
<div class="ad"></div>
"#;

  let mut renderer = FastRender::new().expect("renderer");
  let dom = renderer.parse_html(html).expect("parse html");
  let fragment = renderer.layout_document(&dom, 200, 60).expect("layout");

  let texts = fragment
    .iter_fragments()
    .filter_map(|frag| match &frag.content {
      FragmentContent::Text { text, .. } => Some(text.to_string()),
      _ => None,
    })
    .collect::<Vec<_>>()
    .join(" ");

  assert!(
    texts.contains("ADVERTISEMENT"),
    "expected generated pseudo-element content text; got: {texts:?}"
  );
}

#[test]
fn flex_centered_zero_width_item_does_not_shift_intrinsic_fallback_layout() {
  // Regression for flex child assembly: when a flex item resolves to 0px main size (common for
  // empty ad containers) but we still need a non-zero layout to compute absolute children, we must
  // not treat the 0px origin as the start edge of the expanded subtree.
  let html = r#"<!doctype html>
<style>
  html, body { margin: 0; padding: 0; background: #fff; }
  .outer { display: flex; justify-content: center; width: 100px; height: 20px; background: #fff; }
  .ad { display: flex; justify-content: center; position: relative; }
  .ad::before { content: ""; position: absolute; width: 40px; height: 10px; background: #f00; }
  .spacer { height: 10px; }
</style>
<div class="outer"><div class="ad"><div class="spacer"></div></div></div>
"#;

  let mut renderer = FastRender::new().expect("renderer");
  let pixmap = renderer.render_html(html, 100, 20).expect("render");

  let red = (255, 0, 0, 255);
  let bbox = find_exact_color_bbox(&pixmap, red).expect("expected red pixels");

  // The 40px bar should be centered within the 100px container: left=(100-40)/2=30.
  assert_eq!((bbox.0, bbox.2), (30, 69), "unexpected red bbox: {bbox:?}");
}

#[test]
fn flex_centered_zero_height_item_does_not_shift_intrinsic_fallback_layout() {
  // Like the main-axis regression above, but for cross-axis alignment. When a flex item resolves to
  // 0px cross size but we later inflate it during layout (e.g. because it only contains absolutely
  // positioned pseudo-elements), the child's origin must be adjusted based on `align-items`/
  // `align-self`.
  let html = r#"<!doctype html>
<style>
  html, body { margin: 0; padding: 0; background: #fff; }
  .outer { display: flex; align-items: center; width: 20px; height: 100px; background: #fff; }
  .ad { position: relative; }
  .ad::before { content: ""; position: absolute; width: 10px; height: 40px; background: #f00; }
</style>
<div class="outer"><div class="ad"></div></div>
"#;

  let mut renderer = FastRender::new().expect("renderer");
  let pixmap = renderer.render_html(html, 20, 100).expect("render");

  let red = (255, 0, 0, 255);
  let bbox = find_exact_color_bbox(&pixmap, red).expect("expected red pixels");

  // The 40px bar should be centered vertically within the 100px container: top=(100-40)/2=30.
  assert_eq!((bbox.1, bbox.3), (30, 69), "unexpected red bbox: {bbox:?}");
}

#[test]
fn near_fit_text_does_not_wrap_due_to_subpixel_rounding() {
  // Regression for line breaking: slight float rounding differences between the available width and
  // shaped advances can incorrectly split a breakable word onto multiple lines.
  //
  // Measure the text's intrinsic advance and then set the available width to just barely smaller
  // than that advance. This should still be treated as "fits" (within epsilon), so the text remains
  // on a single line.
  let text = "ADVERTISEMENT";

  let mut renderer = FastRender::new().expect("renderer");

  let wide_html = format!(
    r#"<!doctype html>
<style>
  html, body {{ margin: 0; padding: 0; background: #fff; }}
  .box {{ width: 1000px; overflow-wrap: anywhere; font-size: 12px; }}
</style>
<div class="box">{text}</div>
"#
  );
  let wide_dom = renderer.parse_html(&wide_html).expect("parse html");
  let wide_fragment = renderer.layout_document(&wide_dom, 1000, 200).expect("layout");
  let (measured_width, text_frag_count) = wide_fragment
    .iter_fragments()
    .filter_map(|frag| match &frag.content {
      FragmentContent::Text { text: frag_text, .. } if frag_text.as_ref() == text => {
        Some((frag.bounds.size.width, 1usize))
      }
      _ => None,
    })
    .fold((0.0f32, 0usize), |(w, c), (frag_w, frag_c)| (w.max(frag_w), c + frag_c));
  assert_eq!(
    text_frag_count, 1,
    "expected single text fragment in wide layout; got {text_frag_count}"
  );

  let near_fit_width = (measured_width - 0.1).max(0.0);
  let narrow_html = format!(
    r#"<!doctype html>
<style>
  html, body {{ margin: 0; padding: 0; background: #fff; }}
  .box {{ width: {near_fit_width}px; overflow-wrap: anywhere; font-size: 12px; }}
</style>
<div class="box">{text}</div>
"#
  );

  let narrow_dom = renderer.parse_html(&narrow_html).expect("parse html");
  let narrow_fragment = renderer
    .layout_document(&narrow_dom, 1000, 200)
    .expect("layout");

  let text_fragments = narrow_fragment
    .iter_fragments()
    .filter_map(|frag| match &frag.content {
      FragmentContent::Text { text, .. } => Some(text.to_string()),
      _ => None,
    })
    .collect::<Vec<_>>();
  let full_count = text_fragments.iter().filter(|t| t.as_str() == text).count();
  assert_eq!(
    full_count, 1,
    "expected near-fit text to stay on one line; fragments={text_fragments:?} measured_width={measured_width:.3} near_fit_width={near_fit_width:.3}"
  );
}
