use crate::geometry::{Point, Rect};
use crate::style::types::{FontSizeAdjust, FontSizeAdjustMetric};
use crate::text::font_db::compute_font_size_adjusted_size;
use crate::tree::fragment_tree::{FragmentContent, FragmentNode};
use crate::{FastRender, FastRenderConfig, FontConfig, FontContext};

fn find_deepest_block_fragment_abs_by_text(fragment: &FragmentNode, needle: &str) -> Option<Rect> {
  fn rec(
    node: &FragmentNode,
    needle: &str,
    origin: Point,
    depth: usize,
  ) -> (bool, Option<(usize, Rect)>) {
    let node_origin = origin.translate(node.bounds.origin);

    let mut contains = matches!(
      &node.content,
      FragmentContent::Text { text, .. } if text.as_ref().contains(needle)
    );
    let mut best: Option<(usize, Rect)> = None;

    for child in node.children.iter() {
      let (child_contains, child_best) = rec(child, needle, node_origin, depth + 1);
      contains |= child_contains;
      if let Some(candidate) = child_best {
        best = match best {
          Some(current) if current.0 >= candidate.0 => Some(current),
          _ => Some(candidate),
        };
      }
    }

    if contains && node.content.is_block() {
      let abs_rect = Rect::new(node_origin, node.bounds.size);
      let candidate = (depth, abs_rect);
      best = match best {
        Some(current) if current.0 >= candidate.0 => Some(current),
        _ => Some(candidate),
      };
    }

    (contains, best)
  }

  rec(fragment, needle, Point::ZERO, 0)
    .1
    .map(|(_, rect)| rect)
}

#[test]
fn root_relative_font_units_use_root_font_metrics_and_used_line_height() {
  let config = FastRenderConfig::default().with_font_sources(FontConfig::bundled_only());
  let mut renderer = FastRender::with_config(config).expect("renderer");

  // Use a non-default font-size-adjust to ensure root font metrics differ materially from the
  // deterministic fallback ratios used when metrics are unavailable (rch≈0.5em, rcap≈0.7em).
  //
  // Also override the child element font-family so r* units must resolve against the root font,
  // not the element's own font.
  // Keep the root font distinct from the body font so `r*` units must resolve against the root,
  // not the element’s own font. Use a flex-row probe to measure `1rch` by observing where the
  // following marker element is placed.
  let html = r##"
    <html style="font-family:'Roboto Flex'; font-size:20px; font-size-adjust:1; line-height:2;">
      <body style="margin:0; padding:0; font-family:'Noto Serif';">
        <div style="font-family:'Roboto Flex'">0</div>
        <div style="display:flex; flex-direction:row; align-items:flex-start;">
          <div style="flex:0 0 auto; width:1rch; height:1px;"></div><div style="flex:0 0 auto;">RCH_MARK</div>
        </div>
        <div style="margin-left:1rcap; white-space:nowrap;">RCAP_MARK</div>
        <div style="position:relative; height:200px;">RLH_CONTAINER<div style="position:absolute; top:1rlh; left:0; white-space:nowrap;">RLH_MARK</div></div>
      </body>
    </html>
  "##;

  let dom = renderer.parse_html(html).expect("parse");
  let tree = renderer.layout_document(&dom, 400, 300).expect("layout");

  let rch_marker_rect = find_deepest_block_fragment_abs_by_text(&tree.root, "RCH_MARK")
    .expect("rch marker block fragment");
  let rch_marker_x = rch_marker_rect.x();

  let root_font_size_px = 20.0_f32;

  let cap_rect = find_deepest_block_fragment_abs_by_text(&tree.root, "RCAP_MARK")
    .expect("rcap element block fragment");

  // Compute the expected root font-relative metrics in px from the bundled Roboto Flex face.
  let font_ctx = FontContext::with_config(FontConfig::bundled_only());
  let families = vec!["Roboto Flex".to_string()];
  let font = font_ctx
    .get_font_full(
      &families,
      400,
      crate::text::font_db::FontStyle::Normal,
      crate::text::font_db::FontStretch::Normal,
    )
    .expect("Roboto Flex loaded");
  let used_size = compute_font_size_adjusted_size(
    root_font_size_px,
    FontSizeAdjust::Number {
      ratio: 1.0,
      metric: FontSizeAdjustMetric::ExHeight,
    },
    &font,
    None,
  );
  let expected_cap_px =
    font.font_size_adjust_metric_ratio_or_fallback(FontSizeAdjustMetric::CapHeight) * used_size;
  let expected_ch_px =
    font.font_size_adjust_metric_ratio_or_fallback(FontSizeAdjustMetric::ChWidth) * used_size;

  // Ensure the chosen root font+adjustment actually deviates from the old deterministic fallback
  // (rch≈0.5em) so this test differentiates the regression.
  assert!(
    (expected_ch_px - (root_font_size_px * 0.5)).abs() > 1.0,
    "expected root ch advance ({expected_ch_px}) to differ materially from fallback 0.5em ({})",
    root_font_size_px * 0.5
  );

  assert!(
    (rch_marker_x - expected_ch_px).abs() < 0.1,
    "expected width:1rch to offset the marker by {expected_ch_px}px, got {rch_marker_x}"
  );

  assert!(
    (expected_cap_px - (root_font_size_px * 0.7)).abs() > 1.0,
    "expected root cap-height ({expected_cap_px}) to differ materially from fallback 0.7em ({})",
    root_font_size_px * 0.7
  );

  assert!(
    (cap_rect.x() - expected_cap_px).abs() < 0.1,
    "expected margin-left:1rcap to offset by {expected_cap_px}px, got {}px",
    cap_rect.x()
  );

  let rlh_container_rect = find_deepest_block_fragment_abs_by_text(&tree.root, "RLH_CONTAINER")
    .expect("rlh container block fragment");
  let rlh_rect = find_deepest_block_fragment_abs_by_text(&tree.root, "RLH_MARK")
    .expect("rlh element block fragment");
  let actual_top = rlh_rect.y() - rlh_container_rect.y();
  let expected_margin_top = root_font_size_px * 2.0;

  assert!(
    (actual_top - expected_margin_top).abs() < 0.1,
    "expected top:1rlh to equal {expected_margin_top}px, got {actual_top}px"
  );
}
