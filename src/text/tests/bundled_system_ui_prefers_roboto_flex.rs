use crate::{ComputedStyle, FontConfig, FontContext, ShapingPipeline};

#[test]
fn bundled_system_ui_prefers_roboto_flex_over_dejavu_sans() {
  // Regression test for pages/fixtures that use `system-ui` as the primary font-family (e.g.
  // individual_transforms, hidden_attribute_display_none).
  //
  // In bundled-font mode (no system fonts) the `system-ui` generic used to resolve to the bundled
  // DejaVu Sans face because it appears in the `system-ui` fallback list. DejaVu Sans' wider
  // metrics caused wrap-driven layout drift compared to Chrome on typical Linux configs.
  //
  // Prefer Roboto Flex (our bundled sans fallback) by aliasing `Roboto` → `Roboto Flex` in bundled
  // mode, so `system-ui` picks a narrower face before reaching DejaVu Sans.
  let ctx = FontContext::with_config(FontConfig::bundled_only());

  let mut style = ComputedStyle::default();
  style.font_family = vec!["system-ui".to_string()].into();
  style.font_size = 16.0;

  let runs = ShapingPipeline::new()
    .shape("System UI fallback", &style, &ctx)
    .expect("shaping succeeds");
  assert!(!runs.is_empty(), "expected at least one shaped run");
  assert_eq!(
    runs[0].font.family, "Roboto Flex",
    "expected bundled system-ui to resolve to Roboto Flex"
  );
}
