use fastrender::debug::runtime::RuntimeToggles;
use fastrender::paint::display_list_renderer::PaintParallelism;
use fastrender::{FastRender, FastRenderConfig, FontConfig, LayoutParallelism};
use rayon::ThreadPoolBuilder;
use std::collections::HashMap;
use std::sync::Once;
use tiny_skia::Pixmap;

const VIEWPORT_WIDTH: u32 = 200;
const VIEWPORT_HEIGHT: u32 = 120;

fn init_rayon_for_tests() {
  static INIT: Once = Once::new();
  INIT.call_once(|| {
    // Rayon defaults to spawning one worker per CPU; in constrained environments this can fail
    // global pool initialization (EAGAIN). Pre-initialize a conservative pool so paint tests are
    // stable under `scripts/run_limited.sh`.
    std::env::set_var("RAYON_NUM_THREADS", "2");
    let _ = ThreadPoolBuilder::new().num_threads(2).build_global();
  });
}

fn create_renderer() -> FastRender {
  init_rayon_for_tests();
  let toggles = RuntimeToggles::from_map(HashMap::from([(
    "FASTR_PAINT_BACKEND".to_string(),
    "display_list".to_string(),
  )]));
  let config = FastRenderConfig::new()
    .with_runtime_toggles(toggles)
    .with_font_sources(FontConfig::bundled_only())
    .with_layout_parallelism(LayoutParallelism::disabled())
    .with_paint_parallelism(PaintParallelism::disabled());
  FastRender::with_config(config).expect("renderer")
}

fn fixture(isolated: bool) -> String {
  let isolation = if isolated { "isolation: isolate;" } else { "" };
  format!(
    r#"
    <style>
      body {{ margin: 0; background: black; }}
      #t {{
        position: absolute;
        left: 80px;
        top: 40px;
        font: 40px/1 sans-serif;
        color: transparent;
        text-shadow: -50px 0 0 rgb(255, 0, 0);
        {isolation}
      }}
    </style>
    <div id="t">A</div>
  "#
  )
}

fn assert_pixmap_eq(label: &str, expected: &Pixmap, actual: &Pixmap) {
  assert_eq!(expected.width(), actual.width(), "{label}: width mismatch");
  assert_eq!(expected.height(), actual.height(), "{label}: height mismatch");

  let expected_data = expected.data();
  let actual_data = actual.data();
  if expected_data == actual_data {
    return;
  }

  let width = expected.width() as usize;
  let height = expected.height() as usize;
  let mut first: Option<(usize, usize, [u8; 4], [u8; 4])> = None;
  let mut diff_min_x = usize::MAX;
  let mut diff_min_y = usize::MAX;
  let mut diff_max_x = 0usize;
  let mut diff_max_y = 0usize;
  let mut diff_pixels = 0usize;

  for y in 0..height {
    for x in 0..width {
      let idx = (y * width + x) * 4;
      let e = &expected_data[idx..idx + 4];
      let a = &actual_data[idx..idx + 4];
      if e == a {
        continue;
      }
      diff_pixels += 1;
      diff_min_x = diff_min_x.min(x);
      diff_min_y = diff_min_y.min(y);
      diff_max_x = diff_max_x.max(x);
      diff_max_y = diff_max_y.max(y);
      if first.is_none() {
        first = Some((x, y, e.try_into().unwrap(), a.try_into().unwrap()));
      }
    }
  }

  if let Some((x, y, e, a)) = first {
    panic!(
      "{label}: {diff_pixels} pixels differ; diff_bbox=({diff_min_x},{diff_min_y})-({diff_max_x},{diff_max_y}); first at ({x},{y}) expected={e:?} actual={a:?}"
    );
  }

  panic!("{label}: pixmaps differ, but could not locate mismatch");
}

#[test]
fn stacking_context_bounded_layer_includes_text_shadow_overflow() {
  let baseline_html = fixture(false);
  let isolated_html = fixture(true);

  let mut renderer = create_renderer();
  let baseline = renderer
    .render_html(&baseline_html, VIEWPORT_WIDTH, VIEWPORT_HEIGHT)
    .expect("baseline render");

  let non_black = baseline
    .pixels()
    .iter()
    .filter(|px| px.red() != 0 || px.green() != 0 || px.blue() != 0)
    .count();
  assert!(
    non_black > 0,
    "baseline render should contain at least one non-black pixel (got {non_black})"
  );

  let isolated = renderer
    .render_html(&isolated_html, VIEWPORT_WIDTH, VIEWPORT_HEIGHT)
    .expect("isolated render");

  assert_pixmap_eq(
    "stacking context bounded layers should not clip text-shadow paint overflow",
    &baseline,
    &isolated,
  );
}
