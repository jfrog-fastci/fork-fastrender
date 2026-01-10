use fastrender::css::properties::parse_length;
use fastrender::layout::utils::resolve_length_with_percentage_metrics;
use fastrender::style::types::FontSizeAdjustMetric;
use fastrender::style::values::{Length, LengthUnit};
use fastrender::tree::fragment_tree::FragmentContent;
use fastrender::{FastRender, FastRenderConfig, FontConfig, FontContext, Size};

fn approx_eq(actual: f32, expected: f32, epsilon: f32, msg: &str) {
  assert!(
    (actual - expected).abs() <= epsilon,
    "{msg}: got {actual}, expected {expected} (eps={epsilon})"
  );
}

fn find_text_fragment_pos(
  fragment: &fastrender::FragmentNode,
  needle: &str,
) -> Option<(f32, f32)> {
  fn walk(
    node: &fastrender::FragmentNode,
    origin: (f32, f32),
    needle: &str,
  ) -> Option<(f32, f32)> {
    let abs_x = origin.0 + node.bounds.x();
    let abs_y = origin.1 + node.bounds.y();
    if let FragmentContent::Text { text, .. } = &node.content {
      if text.contains(needle) {
        return Some((abs_x, abs_y));
      }
    }
    for child in node.children.iter() {
      if let Some(found) = walk(child, (abs_x, abs_y), needle) {
        return Some(found);
      }
    }
    None
  }

  walk(fragment, (0.0, 0.0), needle)
}

#[test]
fn resolve_cap_length_falls_back_to_0_7_em() {
  let viewport = Size::new(800.0, 600.0);
  let resolved = resolve_length_with_percentage_metrics(
    Length::new(1.0, LengthUnit::Cap),
    None,
    viewport,
    20.0,
    20.0,
    None,
    None,
  )
  .expect("resolved cap length");
  approx_eq(resolved, 14.0, 0.01, "1cap should fall back to 0.7em");
}

#[test]
fn resolve_ic_length_falls_back_to_1em() {
  let viewport = Size::new(800.0, 600.0);
  let resolved = resolve_length_with_percentage_metrics(
    Length::new(1.0, LengthUnit::Ic),
    None,
    viewport,
    20.0,
    20.0,
    None,
    None,
  )
  .expect("resolved ic length");
  approx_eq(resolved, 20.0, 0.01, "1ic should fall back to 1em");
}

#[test]
fn resolve_root_font_relative_units_use_root_font_size() {
  let viewport = Size::new(800.0, 600.0);

  let resolved = resolve_length_with_percentage_metrics(
    Length::new(1.0, LengthUnit::Rcap),
    None,
    viewport,
    10.0,
    20.0,
    None,
    None,
  )
  .expect("resolved rcap length");
  approx_eq(
    resolved,
    14.0,
    0.01,
    "1rcap should resolve against root font size (0.7 * root font-size)",
  );

  let resolved = resolve_length_with_percentage_metrics(
    Length::new(1.0, LengthUnit::Rex),
    None,
    viewport,
    10.0,
    20.0,
    None,
    None,
  )
  .expect("resolved rex length");
  approx_eq(
    resolved,
    10.0,
    0.01,
    "1rex should resolve against root font size (0.5 * root font-size)",
  );

  let resolved = resolve_length_with_percentage_metrics(
    Length::new(1.0, LengthUnit::Rlh),
    None,
    viewport,
    10.0,
    20.0,
    None,
    None,
  )
  .expect("resolved rlh length");
  approx_eq(
    resolved,
    24.0,
    0.01,
    "1rlh should resolve against root font size (1.2 * root font-size)",
  );
}

#[test]
fn calc_with_cap_terms_resolves() {
  let viewport = Size::new(800.0, 600.0);
  let len = parse_length("calc(1cap + 2px)").expect("calc length parses");
  let resolved =
    resolve_length_with_percentage_metrics(len, None, viewport, 20.0, 20.0, None, None)
      .expect("resolved calc length");
  approx_eq(
    resolved,
    16.0,
    0.01,
    "calc(1cap + 2px) should resolve with cap fallback (0.7em + 2px)",
  );
}

#[test]
fn margin_left_cap_differs_from_em_with_font_metrics() {
  let config = FastRenderConfig::default().with_font_sources(FontConfig::bundled_only());
  let mut renderer = FastRender::with_config(config).expect("renderer");

  let font_size = 20.0f32;
  let html = format!(
    r#"
      <html>
        <body style="margin:0; font-size:{font_size}px; font-family:sans-serif">
          <div style="margin-left:1cap">CAP_UNIT</div>
          <div style="margin-left:1em">EM_UNIT</div>
        </body>
      </html>
    "#
  );

  let dom = renderer.parse_html(&html).expect("parse");
  let tree = renderer.layout_document(&dom, 200, 200).expect("layout");

  let (cap_x, _) = find_text_fragment_pos(&tree.root, "CAP_UNIT").expect("cap text fragment");
  let (em_x, _) = find_text_fragment_pos(&tree.root, "EM_UNIT").expect("em text fragment");

  // Compute the expected cap height from the bundled sans-serif font metrics.
  let font_ctx = FontContext::with_config(FontConfig::bundled_only());
  let font = font_ctx.get_sans_serif().expect("bundled sans-serif font");
  let cap_ratio = font.font_size_adjust_metric_ratio_or_fallback(FontSizeAdjustMetric::CapHeight);
  let expected_cap = cap_ratio * font_size;

  approx_eq(
    em_x,
    font_size,
    0.5,
    "1em margin-left should equal font-size",
  );
  approx_eq(
    cap_x,
    expected_cap,
    0.5,
    "1cap margin-left should use cap metric",
  );

  if (cap_ratio - 1.0).abs() > 1e-3 {
    assert!(
      (cap_x - em_x).abs() > 0.1,
      "expected 1cap and 1em to differ when cap-height ratio != 1.0 (cap_ratio={cap_ratio}, cap_x={cap_x}, em_x={em_x})"
    );
  }
}
