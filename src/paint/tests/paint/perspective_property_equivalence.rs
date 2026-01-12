use crate::paint::display_list::{DisplayItem, StackingContextItem, Transform3D};
use crate::text::font_db::FontConfig;
use crate::{
  FastRender, LayoutParallelism, PaintParallelism, RenderArtifactRequest, RenderArtifacts,
  RenderOptions,
};

fn collect_stacking_contexts(list: &[DisplayItem]) -> Vec<StackingContextItem> {
  list
    .iter()
    .filter_map(|item| match item {
      DisplayItem::PushStackingContext(sc) => Some(sc.clone()),
      _ => None,
    })
    .collect()
}

fn max_abs_matrix_delta(a: &Transform3D, b: &Transform3D) -> f32 {
  a.m
    .iter()
    .zip(b.m.iter())
    .map(|(x, y)| (*x - *y).abs())
    .fold(0.0, f32::max)
}

fn classify_layer(transform: &Transform3D) -> i32 {
  // A `rotateY()` transform flips the sign of the X basis vector's Z component.
  //
  // Use `m[2]` (m20) rather than `m[8]` (m02): once a perspective projection is included, `m[8]`
  // no longer reliably preserves the sign needed to distinguish the front/back planes.
  //
  // In this WPT fixture:
  // - front rotates by -12deg ⇒ sin(angle) < 0 ⇒ -sin(angle) > 0 ⇒ m[2] > 0
  // - back rotates by +14deg  ⇒ sin(angle) > 0 ⇒ -sin(angle) < 0 ⇒ m[2] < 0
  if transform.m[2] < 0.0 { -1 } else { 1 }
}

#[test]
fn perspective_property_matches_perspective_transform_function() {
  let html = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/wpt/tests/transforms/perspective-preserve-3d-001.html"
  ));
  let html_ref = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/wpt/tests/transforms/perspective-preserve-3d-001-ref.html"
  ));

  let mut renderer = FastRender::builder()
    .font_sources(FontConfig::bundled_only())
    .build()
    .expect("renderer");
  let options = RenderOptions::new()
    .with_viewport(440, 340)
    .with_paint_parallelism(PaintParallelism::disabled())
    .with_layout_parallelism(LayoutParallelism::disabled());

  let mut artifacts_test = RenderArtifacts::new(RenderArtifactRequest {
    display_list: true,
    ..RenderArtifactRequest::none()
  });
  renderer
    .render_html_with_options_and_artifacts(html, options.clone(), &mut artifacts_test)
    .expect("render test html");

  let mut artifacts_ref = RenderArtifacts::new(RenderArtifactRequest {
    display_list: true,
    ..RenderArtifactRequest::none()
  });
  renderer
    .render_html_with_options_and_artifacts(html_ref, options, &mut artifacts_ref)
    .expect("render reference html");

  let list_test = artifacts_test
    .display_list
    .as_ref()
    .expect("test display list");
  let list_ref = artifacts_ref
    .display_list
    .as_ref()
    .expect("ref display list");

  let contexts_test = collect_stacking_contexts(list_test.items());
  let contexts_ref = collect_stacking_contexts(list_ref.items());

  let stage_test = contexts_test
    .iter()
    .find(|sc| {
      sc.child_perspective.is_some()
        && (sc.plane_rect.width() - 276.0).abs() < 1e-3
        && (sc.plane_rect.height() - 216.0).abs() < 1e-3
    })
    .expect("stage stacking context with child perspective");
  let stage_perspective = stage_test
    .child_perspective
    .as_ref()
    .expect("stage child perspective");
  assert!(
    stage_test.transform.is_none(),
    "expected stage stacking context to have no self transform"
  );

  let mut layers_test: Vec<&StackingContextItem> = contexts_test
    .iter()
    .filter(|sc| {
      sc.transform.is_some()
        && (sc.plane_rect.width() - 240.0).abs() < 1e-3
        && (sc.plane_rect.height() - 180.0).abs() < 1e-3
    })
    .collect();
  let mut layers_ref: Vec<&StackingContextItem> = contexts_ref
    .iter()
    .filter(|sc| {
      sc.transform.is_some()
        && (sc.plane_rect.width() - 240.0).abs() < 1e-3
        && (sc.plane_rect.height() - 180.0).abs() < 1e-3
    })
    .collect();

  assert_eq!(layers_test.len(), 2, "expected two layer stacking contexts");
  assert_eq!(layers_ref.len(), 2, "expected two layer stacking contexts");

  layers_test.sort_by_key(|sc| classify_layer(sc.transform.as_ref().unwrap()));
  layers_ref.sort_by_key(|sc| classify_layer(sc.transform.as_ref().unwrap()));

  for (layer_test, layer_ref) in layers_test.into_iter().zip(layers_ref.into_iter()) {
    let test_transform = layer_test.transform.as_ref().expect("layer transform");
    let ref_transform = layer_ref.transform.as_ref().expect("layer ref transform");
    let combined_post = stage_perspective.multiply(test_transform);
    let combined_opt = stage_perspective.multiply_perspective_optimized(test_transform);
    let combined_pre = test_transform.multiply(stage_perspective);
    let max_delta = max_abs_matrix_delta(&combined_post, ref_transform);
    let max_delta_opt = max_abs_matrix_delta(&combined_opt, ref_transform);
    let max_delta_pre = max_abs_matrix_delta(&combined_pre, ref_transform);
    if max_delta >= 1e-4 {
      eprintln!("stage plane_rect={:?}", stage_test.plane_rect);
      eprintln!("stage child_perspective={:?}", stage_perspective.m);
      eprintln!(
        "layer(test) plane_rect={:?} transform={:?}",
        layer_test.plane_rect, test_transform.m
      );
      eprintln!("layer(combined post) transform={:?}", combined_post.m);
      eprintln!("layer(combined opt) transform={:?}", combined_opt.m);
      eprintln!("layer(combined pre) transform={:?}", combined_pre.m);
      eprintln!(
        "layer(ref) plane_rect={:?} transform={:?}",
        layer_ref.plane_rect, ref_transform.m
      );
      eprintln!("max |Δ| post={max_delta} opt={max_delta_opt} pre={max_delta_pre}");
    }
    assert!(
      max_delta_opt < 1e-6,
      "expected optimized perspective multiplication to match perspective() transform function; max |Δ|={max_delta_opt}"
    );
    assert!(
      max_delta < 1e-4,
      "expected layer transform with parent perspective to match perspective() transform function; max |Δ|={max_delta}"
    );
  }
}
