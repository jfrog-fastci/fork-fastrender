use crate::style::types::FontWeight;
use crate::{ComputedStyle, FontConfig, FontContext, ShapingPipeline};

fn shaped_run_info(text: &str, style: &ComputedStyle, ctx: &FontContext) -> (String, f32) {
  let runs = ShapingPipeline::new()
    .shape(text, style, ctx)
    .expect("shaping succeeds");
  assert!(
    !runs.is_empty(),
    "expected shaping to produce at least one run"
  );
  let family = runs[0].font.family.clone();
  let advance: f32 = runs.iter().map(|r| r.advance).sum();
  (family, advance)
}

#[test]
fn bundled_helvetica_neue_alias_prefers_roboto_flex_to_reduce_wrap_drift() {
  // Regression test for the reddit.com offline fixture (blocked page).
  //
  // The fixture uses a cross-platform font stack that eventually falls back to:
  //   "Helvetica Neue", Arial, sans-serif
  // In the bundled-font configuration we don't ship Helvetica/Arial, so we use deterministic
  // aliases. Prefer Roboto Flex over Noto Sans because its metrics are closer to Chrome's typical
  // Linux sans-serif fallback and reduce wrap-driven layout drift on narrow containers.
  let ctx = FontContext::with_config(FontConfig::bundled_only());

  // Chosen to be just narrow enough that Noto Sans would wrap in the fixture container
  // (~472px available width at the default viewport), while Roboto Flex fits on one line.
  const AVAILABLE_WIDTH: f32 = 472.0;
  let text = "You've been blocked by network security.";

  let mut noto_style = ComputedStyle::default();
  noto_style.font_family = vec!["Noto Sans".to_string()].into();
  noto_style.font_size = 24.0;
  noto_style.font_weight = FontWeight::Number(700);
  let (noto_family, noto_advance) = shaped_run_info(text, &noto_style, &ctx);
  assert_eq!(noto_family, "Noto Sans");
  assert!(
    noto_advance > AVAILABLE_WIDTH,
    "expected Noto Sans to exceed {AVAILABLE_WIDTH}px so it would wrap (got {noto_advance:.3})"
  );

  let mut roboto_style = ComputedStyle::default();
  roboto_style.font_family = vec!["Roboto Flex".to_string()].into();
  roboto_style.font_size = 24.0;
  roboto_style.font_weight = FontWeight::Number(700);
  let (roboto_family, roboto_advance) = shaped_run_info(text, &roboto_style, &ctx);
  assert_eq!(roboto_family, "Roboto Flex");
  assert!(
    roboto_advance <= AVAILABLE_WIDTH,
    "expected Roboto Flex to fit within {AVAILABLE_WIDTH}px (got {roboto_advance:.3})"
  );

  let mut aliased_style = ComputedStyle::default();
  aliased_style.font_family = vec!["Helvetica Neue".to_string()].into();
  aliased_style.font_size = 24.0;
  aliased_style.font_weight = FontWeight::Number(700);
  let (alias_family, alias_advance) = shaped_run_info(text, &aliased_style, &ctx);
  assert_eq!(
    alias_family, "Roboto Flex",
    "expected bundled Helvetica Neue alias to resolve to Roboto Flex"
  );
  assert!(
    alias_advance <= AVAILABLE_WIDTH,
    "expected aliased font to fit within {AVAILABLE_WIDTH}px (got {alias_advance:.3})"
  );
}
