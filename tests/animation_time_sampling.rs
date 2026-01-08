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
fn time_based_opacity_animation_samples_at_multiple_timestamps_and_settles_without_time() {
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
        animation: fade 1000ms linear forwards;
      }
      @keyframes fade { from { opacity: 0; } to { opacity: 1; } }
    </style>
    <div id="box"></div>
  "#;

  let prepared = renderer.prepare_html(html, options).expect("prepare");
  let bg = Rgba::new(0, 0, 0, 1.0);

  let pixmap_0 = prepared
    .paint_with_options(
      PreparedPaintOptions::new()
        .with_background(bg)
        .with_animation_time(0.0),
    )
    .expect("paint at 0ms");
  assert_eq!(pixel(&pixmap_0, 5, 5), (0, 0, 0, 255));

  let pixmap_500 = prepared
    .paint_with_options(
      PreparedPaintOptions::new()
        .with_background(bg)
        .with_animation_time(500.0),
    )
    .expect("paint at 500ms");
  let (r, g, b, a) = pixel(&pixmap_500, 5, 5);
  assert!(
    (120..=135).contains(&r),
    "expected ~50% blended red at 500ms, got rgba=({r},{g},{b},{a})"
  );
  assert_eq!((g, b, a), (0, 0, 255));

  let pixmap_1000 = prepared
    .paint_with_options(
      PreparedPaintOptions::new()
        .with_background(bg)
        .with_animation_time(1000.0),
    )
    .expect("paint at 1000ms");
  assert_eq!(pixel(&pixmap_1000, 5, 5), (255, 0, 0, 255));

  // Without an explicit animation time, finite animations with forwards fill-mode should settle to
  // their post-animation values deterministically.
  let pixmap_settled_a = prepared
    .paint_with_options(PreparedPaintOptions::new().with_background(bg))
    .expect("paint settled A");
  let pixmap_settled_b = prepared
    .paint_with_options(PreparedPaintOptions::new().with_background(bg))
    .expect("paint settled B");
  assert_eq!(pixel(&pixmap_settled_a, 5, 5), (255, 0, 0, 255));
  assert_eq!(pixel(&pixmap_settled_a, 5, 5), pixel(&pixmap_settled_b, 5, 5));
}

#[test]
fn time_based_animation_honors_direction_and_iteration_count() {
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
        /* Two 500ms iterations, alternating direction, for a 1000ms total active duration. */
        animation: fade 500ms linear 0ms 2 alternate forwards;
      }
      @keyframes fade { from { opacity: 0; } to { opacity: 1; } }
    </style>
    <div id="box"></div>
  "#;

  let prepared = renderer.prepare_html(html, options).expect("prepare");
  let bg = Rgba::new(0, 0, 0, 1.0);

  let pixmap_0 = prepared
    .paint_with_options(
      PreparedPaintOptions::new()
        .with_background(bg)
        .with_animation_time(0.0),
    )
    .expect("paint at 0ms");
  assert_eq!(pixel(&pixmap_0, 5, 5), (0, 0, 0, 255));

  // At the end of the first iteration, direction flips and progress is 1.
  let pixmap_500 = prepared
    .paint_with_options(
      PreparedPaintOptions::new()
        .with_background(bg)
        .with_animation_time(500.0),
    )
    .expect("paint at 500ms");
  assert_eq!(pixel(&pixmap_500, 5, 5), (255, 0, 0, 255));

  // With an even iteration-count + alternate direction, the final progress is back at 0.
  let pixmap_1000 = prepared
    .paint_with_options(
      PreparedPaintOptions::new()
        .with_background(bg)
        .with_animation_time(1000.0),
    )
    .expect("paint at 1000ms");
  assert_eq!(pixel(&pixmap_1000, 5, 5), (0, 0, 0, 255));

  // When no explicit time is provided, the settled state should reflect the end progress (0% here
  // because alternate direction ends at the start keyframe).
  let pixmap_settled = prepared
    .paint_with_options(PreparedPaintOptions::new().with_background(bg))
    .expect("paint settled");
  assert_eq!(pixel(&pixmap_settled, 5, 5), (0, 0, 0, 255));
}

#[test]
fn time_based_transform_animation_samples_at_multiple_timestamps() {
  ensure_test_env();
  let mut renderer = FastRender::new().expect("renderer");
  let options = RenderOptions::new().with_viewport(30, 20);
  let html = r#"
    <style>
      html, body { margin: 0; background: rgb(0, 0, 0); }
      #box {
        position: absolute;
        top: 0;
        left: 0;
        width: 10px;
        height: 10px;
        background: rgb(255, 0, 0);
        animation: move 1000ms linear forwards;
      }
      @keyframes move { from { transform: translateX(0px); } to { transform: translateX(10px); } }
    </style>
    <div id="box"></div>
  "#;

  let prepared = renderer.prepare_html(html, options).expect("prepare");
  let bg = Rgba::new(0, 0, 0, 1.0);

  let pixmap_0 = prepared
    .paint_with_options(
      PreparedPaintOptions::new()
        .with_background(bg)
        .with_animation_time(0.0),
    )
    .expect("paint at 0ms");
  assert_eq!(pixel(&pixmap_0, 2, 2), (255, 0, 0, 255));
  assert_eq!(pixel(&pixmap_0, 12, 2), (0, 0, 0, 255));

  let pixmap_500 = prepared
    .paint_with_options(
      PreparedPaintOptions::new()
        .with_background(bg)
        .with_animation_time(500.0),
    )
    .expect("paint at 500ms");
  // At 500ms the element has moved to x=5..15.
  assert_eq!(pixel(&pixmap_500, 2, 2), (0, 0, 0, 255));
  assert_eq!(pixel(&pixmap_500, 7, 2), (255, 0, 0, 255));
  assert_eq!(pixel(&pixmap_500, 14, 2), (255, 0, 0, 255));

  let pixmap_1000 = prepared
    .paint_with_options(
      PreparedPaintOptions::new()
        .with_background(bg)
        .with_animation_time(1000.0),
    )
    .expect("paint at 1000ms");
  // At 1000ms the element has moved to x=10..20.
  assert_eq!(pixel(&pixmap_1000, 7, 2), (0, 0, 0, 255));
  assert_eq!(pixel(&pixmap_1000, 12, 2), (255, 0, 0, 255));

  // No explicit time => deterministic settled end state.
  let pixmap_settled = prepared
    .paint_with_options(PreparedPaintOptions::new().with_background(bg))
    .expect("paint settled");
  assert_eq!(pixel(&pixmap_settled, 12, 2), (255, 0, 0, 255));
}

