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

#[test]
fn time_based_animation_steps_timing_function_is_sampled() {
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
        animation: fade 1000ms steps(2, end) forwards;
      }
      @keyframes fade { from { opacity: 0; } to { opacity: 1; } }
    </style>
    <div id="box"></div>
  "#;

  let prepared = renderer.prepare_html(html, options).expect("prepare");
  let bg = Rgba::new(0, 0, 0, 1.0);

  let pixmap_before_step = prepared
    .paint_with_options(
      PreparedPaintOptions::new()
        .with_background(bg)
        .with_animation_time(450.0),
    )
    .expect("paint before step");
  assert_eq!(pixel(&pixmap_before_step, 5, 5), (0, 0, 0, 255));

  let pixmap_step = prepared
    .paint_with_options(
      PreparedPaintOptions::new()
        .with_background(bg)
        .with_animation_time(500.0),
    )
    .expect("paint at step");
  let (r, g, b, a) = pixel(&pixmap_step, 5, 5);
  assert!(
    (120..=135).contains(&r),
    "expected first step (~0.5) at 500ms, got rgba=({r},{g},{b},{a})"
  );
  assert_eq!((g, b, a), (0, 0, 255));

  let pixmap_late_step = prepared
    .paint_with_options(
      PreparedPaintOptions::new()
        .with_background(bg)
        .with_animation_time(950.0),
    )
    .expect("paint late step");
  let (r, g, b, a) = pixel(&pixmap_late_step, 5, 5);
  assert!(
    (120..=135).contains(&r),
    "expected still on first step (~0.5) before 1000ms, got rgba=({r},{g},{b},{a})"
  );
  assert_eq!((g, b, a), (0, 0, 255));

  let pixmap_end = prepared
    .paint_with_options(
      PreparedPaintOptions::new()
        .with_background(bg)
        .with_animation_time(1000.0),
    )
    .expect("paint at end");
  assert_eq!(pixel(&pixmap_end, 5, 5), (255, 0, 0, 255));
}

#[test]
fn time_based_animation_ease_timing_function_is_non_linear() {
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
        animation: fade 1000ms ease forwards;
      }
      @keyframes fade { from { opacity: 0; } to { opacity: 1; } }
    </style>
    <div id="box"></div>
  "#;

  let prepared = renderer.prepare_html(html, options).expect("prepare");
  let bg = Rgba::new(0, 0, 0, 1.0);

  let pixmap_mid = prepared
    .paint_with_options(
      PreparedPaintOptions::new()
        .with_background(bg)
        .with_animation_time(500.0),
    )
    .expect("paint at 500ms");
  let (r, g, b, a) = pixel(&pixmap_mid, 5, 5);
  assert!(
    (200..=210).contains(&r),
    "expected eased progress (~0.802) at 500ms, got rgba=({r},{g},{b},{a})"
  );
  assert_eq!((g, b, a), (0, 0, 255));
}

#[test]
fn multiple_time_based_animations_apply_in_list_order() {
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
        animation:
          fade1 1000ms linear forwards,
          fade2 1000ms linear forwards;
      }
      @keyframes fade1 { from { opacity: 0; } to { opacity: 1; } }
      @keyframes fade2 { from { opacity: 0; } to { opacity: 0.5; } }
    </style>
    <div id="box"></div>
  "#;

  let prepared = renderer.prepare_html(html, options).expect("prepare");
  let bg = Rgba::new(0, 0, 0, 1.0);

  let pixmap_mid = prepared
    .paint_with_options(
      PreparedPaintOptions::new()
        .with_background(bg)
        .with_animation_time(500.0),
    )
    .expect("paint at 500ms");
  let (r, g, b, a) = pixel(&pixmap_mid, 5, 5);
  assert!(
    (60..=70).contains(&r),
    "expected second animation (0.25) to win at 500ms, got rgba=({r},{g},{b},{a})"
  );
  assert_eq!((g, b, a), (0, 0, 255));

  let pixmap_end = prepared
    .paint_with_options(
      PreparedPaintOptions::new()
        .with_background(bg)
        .with_animation_time(1000.0),
    )
    .expect("paint at 1000ms");
  let (r, g, b, a) = pixel(&pixmap_end, 5, 5);
  assert!(
    (120..=135).contains(&r),
    "expected second animation (0.5) to win at 1000ms, got rgba=({r},{g},{b},{a})"
  );
  assert_eq!((g, b, a), (0, 0, 255));

  // In deterministic "settled" mode, both forwards animations apply and the later one wins.
  let pixmap_settled = prepared
    .paint_with_options(PreparedPaintOptions::new().with_background(bg))
    .expect("paint settled");
  let (r, g, b, a) = pixel(&pixmap_settled, 5, 5);
  assert!(
    (120..=135).contains(&r),
    "expected settled to match second animation end (0.5), got rgba=({r},{g},{b},{a})"
  );
  assert_eq!((g, b, a), (0, 0, 255));
}

#[test]
fn animation_composition_add_adds_transforms() {
  ensure_test_env();
  let mut renderer = FastRender::new().expect("renderer");
  let options = RenderOptions::new().with_viewport(50, 20);
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
        animation:
          a1 1000ms linear forwards,
          a2 1000ms linear forwards;
        animation-composition: replace, add;
      }
      @keyframes a1 { from { transform: translateX(0px); } to { transform: translateX(10px); } }
      @keyframes a2 { from { transform: translateX(0px); } to { transform: translateX(20px); } }
    </style>
    <div id="box"></div>
  "#;

  let prepared = renderer.prepare_html(html, options).expect("prepare");
  let bg = Rgba::new(0, 0, 0, 1.0);

  let pixmap_mid = prepared
    .paint_with_options(
      PreparedPaintOptions::new()
        .with_background(bg)
        .with_animation_time(500.0),
    )
    .expect("paint at 500ms");
  // At 500ms: a1 => +5px, a2 => +10px, add => +15px total.
  assert_eq!(pixel(&pixmap_mid, 12, 2), (0, 0, 0, 255));
  assert_eq!(pixel(&pixmap_mid, 17, 2), (255, 0, 0, 255));

  let pixmap_end = prepared
    .paint_with_options(
      PreparedPaintOptions::new()
        .with_background(bg)
        .with_animation_time(1000.0),
    )
    .expect("paint at 1000ms");
  // At 1000ms: a1 => +10px, a2 => +20px, add => +30px total.
  assert_eq!(pixel(&pixmap_end, 22, 2), (0, 0, 0, 255));
  assert_eq!(pixel(&pixmap_end, 32, 2), (255, 0, 0, 255));

  // Settled mode should match the end state (30px total).
  let pixmap_settled = prepared
    .paint_with_options(PreparedPaintOptions::new().with_background(bg))
    .expect("paint settled");
  assert_eq!(pixel(&pixmap_settled, 32, 2), (255, 0, 0, 255));
}

#[test]
fn animation_delay_and_fill_mode_backwards_apply_start_state_before_delay() {
  ensure_test_env();
  let mut renderer = FastRender::new().expect("renderer");
  let options = RenderOptions::new().with_viewport(30, 20);
  let html = r#"
    <style>
      html, body { margin: 0; background: rgb(0, 0, 0); }
      .box {
        position: absolute;
        top: 0;
        width: 10px;
        height: 10px;
        background: rgb(255, 0, 0);
        animation-name: fade;
        animation-duration: 1000ms;
        animation-delay: 500ms;
        animation-timing-function: linear;
      }
      #none { left: 0; animation-fill-mode: none; }
      #backwards { left: 10px; animation-fill-mode: backwards; }
      @keyframes fade { from { opacity: 0; } to { opacity: 1; } }
    </style>
    <div id="none" class="box"></div>
    <div id="backwards" class="box"></div>
  "#;

  let prepared = renderer.prepare_html(html, options).expect("prepare");
  let bg = Rgba::new(0, 0, 0, 1.0);

  // At 250ms (before the 500ms delay elapses), `fill-mode: backwards` should apply the start
  // keyframe, while `fill-mode: none` should leave the element at its underlying style (opacity: 1).
  let pixmap = prepared
    .paint_with_options(
      PreparedPaintOptions::new()
        .with_background(bg)
        .with_animation_time(250.0),
    )
    .expect("paint");

  assert_eq!(pixel(&pixmap, 5, 5), (255, 0, 0, 255));
  assert_eq!(pixel(&pixmap, 15, 5), (0, 0, 0, 255));
}

#[test]
fn animation_fill_mode_forwards_settles_but_backwards_is_ignored_in_settled_mode() {
  ensure_test_env();
  let mut renderer = FastRender::new().expect("renderer");
  let options = RenderOptions::new().with_viewport(30, 20);
  let html = r#"
    <style>
      html, body { margin: 0; background: rgb(0, 0, 0); }
      .box {
        position: absolute;
        top: 0;
        width: 10px;
        height: 10px;
        background: rgb(255, 0, 0);
        opacity: 1;
        animation: fadeout 1000ms linear;
      }
      #forwards { left: 0; animation-fill-mode: forwards; }
      #backwards { left: 10px; animation-fill-mode: backwards; }
      @keyframes fadeout { from { opacity: 1; } to { opacity: 0; } }
    </style>
    <div id="forwards" class="box"></div>
    <div id="backwards" class="box"></div>
  "#;

  let prepared = renderer.prepare_html(html, options).expect("prepare");
  let bg = Rgba::new(0, 0, 0, 1.0);

  // Deterministic no-time mode should settle only fill-forwards animations to their end state; other
  // fill modes have no deterministic settled state and should fall back to the underlying style.
  let pixmap = prepared
    .paint_with_options(PreparedPaintOptions::new().with_background(bg))
    .expect("paint settled");

  assert_eq!(pixel(&pixmap, 5, 5), (0, 0, 0, 255));
  assert_eq!(pixel(&pixmap, 15, 5), (255, 0, 0, 255));
}

#[test]
fn negative_animation_delay_advances_progress_at_time_zero() {
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
        opacity: 1;
        animation: fadeout 1000ms linear -500ms forwards;
      }
      @keyframes fadeout { from { opacity: 1; } to { opacity: 0; } }
    </style>
    <div id="box"></div>
  "#;

  let prepared = renderer.prepare_html(html, options).expect("prepare");
  let bg = Rgba::new(0, 0, 0, 1.0);

  // With a -500ms delay, at t=0ms the animation is already half-way through.
  let pixmap_0 = prepared
    .paint_with_options(
      PreparedPaintOptions::new()
        .with_background(bg)
        .with_animation_time(0.0),
    )
    .expect("paint at 0ms");
  let (r, g, b, a) = pixel(&pixmap_0, 5, 5);
  assert!(
    (120..=135).contains(&r),
    "expected ~50% faded red at 0ms with -500ms delay, got rgba=({r},{g},{b},{a})"
  );
  assert_eq!((g, b, a), (0, 0, 255));

  // At t=500ms the animation should have completed and (with forwards fill) remain at opacity 0.
  let pixmap_500 = prepared
    .paint_with_options(
      PreparedPaintOptions::new()
        .with_background(bg)
        .with_animation_time(500.0),
    )
    .expect("paint at 500ms");
  assert_eq!(pixel(&pixmap_500, 5, 5), (0, 0, 0, 255));
}

#[test]
fn infinite_iteration_time_based_animation_is_ignored_in_settled_mode() {
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
        animation: fade 1000ms linear infinite;
      }
      @keyframes fade { from { opacity: 0; } to { opacity: 1; } }
    </style>
    <div id="box"></div>
  "#;

  let prepared = renderer.prepare_html(html, options).expect("prepare");
  let bg = Rgba::new(0, 0, 0, 1.0);

  let pixmap_time_0 = prepared
    .paint_with_options(
      PreparedPaintOptions::new()
        .with_background(bg)
        .with_animation_time(0.0),
    )
    .expect("paint at 0ms");
  assert_eq!(pixel(&pixmap_time_0, 5, 5), (0, 0, 0, 255));

  // Without an explicit time, infinite animations cannot be deterministically settled and should be
  // ignored (falling back to the underlying style, which here is opaque red).
  let pixmap_settled = prepared
    .paint_with_options(PreparedPaintOptions::new().with_background(bg))
    .expect("paint settled");
  assert_eq!(pixel(&pixmap_settled, 5, 5), (255, 0, 0, 255));
}

#[test]
fn animations_override_transitions_when_both_apply() {
  ensure_test_env();
  let mut renderer = FastRender::new().expect("renderer");
  let options = RenderOptions::new().with_viewport(20, 20);
  let html = r#"
    <style>
      html, body { margin: 0; background: rgb(0, 0, 0); }

      @starting-style { #box { opacity: 0; } }

      #box {
        width: 10px;
        height: 10px;
        background: rgb(255, 0, 0);
        opacity: 1;
        transition: opacity 1000ms linear;
        animation: anim 1000ms linear both;
      }

      @keyframes anim {
        from { opacity: 0; }
        to { opacity: 0.25; }
      }
    </style>
    <div id="box"></div>
  "#;

  let prepared = renderer.prepare_html(html, options).expect("prepare");
  let bg = Rgba::new(0, 0, 0, 1.0);

  // At 500ms:
  // - transition alone would yield 0.5 opacity
  // - animation yields 0.125 opacity and should win because animations are applied after transitions.
  let pixmap = prepared
    .paint_with_options(
      PreparedPaintOptions::new()
        .with_background(bg)
        .with_animation_time(500.0),
    )
    .expect("paint");
  let (r, g, b, a) = pixel(&pixmap, 5, 5);
  assert!(
    (28..=36).contains(&r),
    "expected animation to override transition at 500ms, got rgba=({r},{g},{b},{a})"
  );
  assert_eq!((g, b, a), (0, 0, 255));
}
