use crate::{ComputedStyle, FontConfig, FontContext, ShapingPipeline};

#[test]
fn bundled_sans_serif_prefers_noto_sans_over_roboto_flex() {
  // Regression test for fixtures (notably Wikipedia) that specify the generic `sans-serif` family
  // directly. In bundled-font mode, choosing a different default sans face than Chrome's system
  // default can cause large text baseline/line-height drift and dominate pixel diffs.
  let ctx = FontContext::with_config(FontConfig::bundled_only());

  let mut style = ComputedStyle::default();
  style.font_family = vec!["sans-serif".to_string()].into();
  style.font_size = 16.0;

  let runs = ShapingPipeline::new()
    .shape("Bundled sans-serif", &style, &ctx)
    .expect("shaping succeeds");
  assert!(!runs.is_empty(), "expected at least one shaped run");
  assert_eq!(
    runs[0].font.family, "Noto Sans",
    "expected bundled sans-serif to resolve to Noto Sans"
  );
}

