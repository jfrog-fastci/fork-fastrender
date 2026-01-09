use fastrender::api::FastRender;
use fastrender::{PreparedPaintOptions, RenderOptions, Rgba};
use std::sync::Once;

static INIT_ENV: Once = Once::new();

fn ensure_test_env() {
  INIT_ENV.call_once(|| {
    // FastRender uses Rayon for parallel layout/paint. Rayon defaults to the host CPU count, which
    // can exceed sandbox thread budgets and cause the global pool init to fail.
    if std::env::var("RAYON_NUM_THREADS").is_err() {
      std::env::set_var("RAYON_NUM_THREADS", "1");
    }
  });
}

fn pixel(pixmap: &fastrender::Pixmap, x: u32, y: u32) -> (u8, u8, u8, u8) {
  let px = pixmap.pixel(x, y).unwrap();
  (px.red(), px.green(), px.blue(), px.alpha())
}

#[test]
fn animation_time_preserves_sub_millisecond_precision() {
  ensure_test_env();
  let mut renderer = FastRender::new().expect("renderer");
  let options = RenderOptions::new().with_viewport(20, 20);
  let html = r#"
    <style>
      html, body { margin: 0; background: rgb(0, 0, 0); }
      #box {
        width: 10px;
        height: 10px;
        background: rgb(255, 0, 0);
        animation: fade 1ms linear forwards;
      }
      @keyframes fade { from { opacity: 0; } to { opacity: 1; } }
    </style>
    <div id="box"></div>
  "#;

  let prepared = renderer.prepare_html(html, options).expect("prepare");
  let bg = Rgba::new(0, 0, 0, 1.0);

  let pixmap_start = prepared
    .paint_with_options(
      PreparedPaintOptions::new()
        .with_background(bg)
        .with_animation_time(0.0),
    )
    .expect("paint at 0ms");
  assert_eq!(pixel(&pixmap_start, 5, 5), (0, 0, 0, 255));

  // Non-finite timestamps should not panic and should behave like 0ms.
  let pixmap_nan = prepared
    .paint_with_options(
      PreparedPaintOptions::new()
        .with_background(bg)
        .with_animation_time(f32::NAN),
    )
    .expect("paint at NaN ms");
  assert_eq!(pixel(&pixmap_nan, 5, 5), (0, 0, 0, 255));

  let pixmap_inf = prepared
    .paint_with_options(
      PreparedPaintOptions::new()
        .with_background(bg)
        .with_animation_time(f32::INFINITY),
    )
    .expect("paint at inf ms");
  assert_eq!(pixel(&pixmap_inf, 5, 5), (0, 0, 0, 255));

  let pixmap_half = prepared
    .paint_with_options(
      PreparedPaintOptions::new()
        .with_background(bg)
        .with_animation_time(0.5),
    )
    .expect("paint at 0.5ms");
  let (r, g, b, a) = pixel(&pixmap_half, 5, 5);
  assert!(
    r > 0 && r < 255,
    "expected partially blended red at 0.5ms, got rgba=({r},{g},{b},{a})"
  );
  assert_eq!(
    (g, b, a),
    (0, 0, 255),
    "expected red blended over black at 0.5ms, got rgba=({r},{g},{b},{a})"
  );

  let pixmap_end = prepared
    .paint_with_options(
      PreparedPaintOptions::new()
        .with_background(bg)
        .with_animation_time(1.0),
    )
    .expect("paint at 1ms");
  assert_eq!(pixel(&pixmap_end, 5, 5), (255, 0, 0, 255));
}
