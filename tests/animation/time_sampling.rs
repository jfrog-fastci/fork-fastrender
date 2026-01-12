use fastrender::{PreparedPaintOptions, RenderOptions, Rgba};

use super::support::{ensure_test_env, pixel, test_renderer};

#[test]
fn time_based_opacity_animation_samples_at_multiple_timestamps_and_settles_without_time() {
  ensure_test_env();
  let mut renderer = test_renderer();
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
  assert_eq!(
    pixel(&pixmap_settled_a, 5, 5),
    pixel(&pixmap_settled_b, 5, 5)
  );
}

#[test]
fn time_based_animation_honors_direction_and_iteration_count() {
  ensure_test_env();
  let mut renderer = test_renderer();
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
  let mut renderer = test_renderer();
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
  let mut renderer = test_renderer();
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
  let mut renderer = test_renderer();
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
  let mut renderer = test_renderer();
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
  let mut renderer = test_renderer();
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
  let mut renderer = test_renderer();
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
  let mut renderer = test_renderer();
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
  let mut renderer = test_renderer();
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
  let mut renderer = test_renderer();
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
  let mut renderer = test_renderer();
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

#[test]
fn animation_play_state_paused_freezes_time_and_resume_preserves_current_time() {
  ensure_test_env();

  use fastrender::animation::{apply_animations_with_state, AnimationStateStore};
  use fastrender::css::parser::parse_stylesheet;
  use fastrender::scroll::ScrollState;
  use fastrender::style::media::MediaContext;
  use fastrender::style::types::{AnimationPlayState, TransitionTimingFunction};
  use fastrender::{ComputedStyle, FragmentNode, FragmentTree, Point, Rect, Size};
  use std::sync::Arc;
  use std::time::Duration;

  let sheet =
    parse_stylesheet("@keyframes fade { from { opacity: 0; } to { opacity: 1; } }").expect("sheet");
  let keyframes = sheet.collect_keyframes(&MediaContext::screen(100.0, 100.0));
  let rule = keyframes
    .into_iter()
    .find(|k| k.name == "fade")
    .expect("fade keyframes");

  let mut style_running = ComputedStyle::default();
  style_running.opacity = 1.0;
  style_running.animation_names = vec![Some("fade".to_string())];
  style_running.animation_durations = vec![1000.0].into();
  style_running.animation_timing_functions = vec![TransitionTimingFunction::Linear].into();
  style_running.animation_play_states = vec![AnimationPlayState::Running].into();

  let mut style_paused = style_running.clone();
  style_paused.animation_play_states = vec![AnimationPlayState::Paused].into();

  let make_tree = |style: ComputedStyle| {
    let mut root =
      FragmentNode::new_block_with_id(Rect::from_xywh(0.0, 0.0, 100.0, 100.0), 1, vec![]);
    root.style = Some(Arc::new(style));
    let mut tree = FragmentTree::with_viewport(root, Size::new(100.0, 100.0));
    tree.keyframes.insert("fade".to_string(), rule.clone());
    tree
  };

  let scroll_state = ScrollState::with_viewport(Point::ZERO);
  let mut store = AnimationStateStore::new();

  // Initialize at t=0 so the timing state represents a document that started at 0ms.
  let mut running_0 = make_tree(style_running.clone());
  apply_animations_with_state(
    &mut running_0,
    &scroll_state,
    Duration::from_millis(0),
    &mut store,
  );
  let opacity_running_0 = running_0.root.style.as_deref().expect("style").opacity;
  assert!(
    opacity_running_0.abs() < 1e-6,
    "expected opacity=0 at t=0ms, got {}",
    opacity_running_0
  );

  // Frame 1: running at t=700ms => opacity 0.7.
  let mut running_700 = make_tree(style_running.clone());
  apply_animations_with_state(
    &mut running_700,
    &scroll_state,
    Duration::from_millis(700),
    &mut store,
  );
  let opacity_running_700 = running_700.root.style.as_deref().expect("style").opacity;
  assert!(
    (opacity_running_700 - 0.7).abs() < 1e-6,
    "expected opacity=0.7, got {}",
    opacity_running_700
  );

  // Frame 2: pause at t=700ms. Value should not jump.
  let mut paused_700 = make_tree(style_paused.clone());
  apply_animations_with_state(
    &mut paused_700,
    &scroll_state,
    Duration::from_millis(700),
    &mut store,
  );
  let opacity_paused_700 = paused_700.root.style.as_deref().expect("style").opacity;
  assert!(
    (opacity_paused_700 - opacity_running_700).abs() < 1e-6,
    "expected pause to preserve opacity at t=700ms (running={}, paused={})",
    opacity_running_700,
    opacity_paused_700
  );

  // Frame 3: still paused at t=900ms. Current time should remain frozen at 700ms.
  let mut paused_900 = make_tree(style_paused.clone());
  apply_animations_with_state(
    &mut paused_900,
    &scroll_state,
    Duration::from_millis(900),
    &mut store,
  );
  let opacity_paused_900 = paused_900.root.style.as_deref().expect("style").opacity;
  assert!(
    (opacity_paused_900 - opacity_running_700).abs() < 1e-6,
    "expected paused opacity to remain frozen (expected={}, got={})",
    opacity_running_700,
    opacity_paused_900
  );

  // Frame 4: resume at t=900ms. Value should still be the paused value at the moment of resuming.
  let mut running_900 = make_tree(style_running.clone());
  apply_animations_with_state(
    &mut running_900,
    &scroll_state,
    Duration::from_millis(900),
    &mut store,
  );
  let opacity_running_900 = running_900.root.style.as_deref().expect("style").opacity;
  assert!(
    (opacity_running_900 - opacity_running_700).abs() < 1e-6,
    "expected resume to start from paused time (expected={}, got={})",
    opacity_running_700,
    opacity_running_900
  );

  // Frame 5: after resuming, time continues to advance from the paused point.
  // Between t=900ms and t=1000ms, only 100ms should elapse on the animation clock:
  // 700ms + 100ms = 800ms => opacity 0.8.
  let mut running_1000 = make_tree(style_running.clone());
  apply_animations_with_state(
    &mut running_1000,
    &scroll_state,
    Duration::from_millis(1000),
    &mut store,
  );
  let opacity_running_1000 = running_1000.root.style.as_deref().expect("style").opacity;
  assert!(
    (opacity_running_1000 - 0.8).abs() < 1e-6,
    "expected opacity=0.8 after resuming, got {}",
    opacity_running_1000
  );
}

#[test]
fn animation_fill_mode_none_switches_to_keyframes_at_delay_boundary() {
  ensure_test_env();
  let mut renderer = test_renderer();
  let options = RenderOptions::new().with_viewport(20, 20);
  let html = r#"
    <style>
      html, body { margin: 0; background: rgb(0, 0, 0); }
      #box {
        width: 10px;
        height: 10px;
        background: rgb(255, 0, 0);
        opacity: 1;
        animation: fade 1000ms linear 500ms none;
      }
      @keyframes fade { from { opacity: 0; } to { opacity: 1; } }
    </style>
    <div id="box"></div>
  "#;

  let prepared = renderer.prepare_html(html, options).expect("prepare");
  let bg = Rgba::new(0, 0, 0, 1.0);

  // Before delay: animation has no effect (fill-mode: none), so the element uses opacity: 1.
  let pixmap_499 = prepared
    .paint_with_options(
      PreparedPaintOptions::new()
        .with_background(bg)
        .with_animation_time(499.0),
    )
    .expect("paint at 499ms");
  assert_eq!(pixel(&pixmap_499, 5, 5), (255, 0, 0, 255));

  // At the delay boundary, the animation becomes active and should sample its start keyframe.
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
fn animation_fill_mode_forwards_applies_end_state_at_end_boundary() {
  ensure_test_env();
  let mut renderer = test_renderer();
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
        opacity: 0;
        animation: fade 1000ms linear;
      }
      #none { left: 0; animation-fill-mode: none; }
      #forwards { left: 10px; animation-fill-mode: forwards; }
      @keyframes fade { from { opacity: 0; } to { opacity: 1; } }
    </style>
    <div id="none" class="box"></div>
    <div id="forwards" class="box"></div>
  "#;

  let prepared = renderer.prepare_html(html, options).expect("prepare");
  let bg = Rgba::new(0, 0, 0, 1.0);

  // At the end boundary, fill-forwards should hold the end keyframe (opacity 1), while fill none
  // should fall back to the underlying style (opacity 0).
  let pixmap = prepared
    .paint_with_options(
      PreparedPaintOptions::new()
        .with_background(bg)
        .with_animation_time(1000.0),
    )
    .expect("paint at 1000ms");

  assert_eq!(pixel(&pixmap, 5, 5), (0, 0, 0, 255));
  assert_eq!(pixel(&pixmap, 15, 5), (255, 0, 0, 255));
}

#[test]
fn starting_style_transition_respects_delay_and_end_boundaries() {
  ensure_test_env();
  let mut renderer = test_renderer();
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
        transition: opacity 1000ms linear 500ms;
      }
    </style>
    <div id="box"></div>
  "#;

  let prepared = renderer.prepare_html(html, options).expect("prepare");
  let bg = Rgba::new(0, 0, 0, 1.0);

  // Before and at the delay boundary, transitions hold their start value (opacity 0).
  for time_ms in [0.0, 499.0, 500.0] {
    let pixmap = prepared
      .paint_with_options(
        PreparedPaintOptions::new()
          .with_background(bg)
          .with_animation_time(time_ms),
      )
      .expect("paint");
    assert_eq!(
      pixel(&pixmap, 5, 5),
      (0, 0, 0, 255),
      "expected start value at t={time_ms}ms"
    );
  }

  // Mid-transition: (1000 - 500) / 1000 == 0.5.
  let pixmap_mid = prepared
    .paint_with_options(
      PreparedPaintOptions::new()
        .with_background(bg)
        .with_animation_time(1000.0),
    )
    .expect("paint mid");
  let (r, g, b, a) = pixel(&pixmap_mid, 5, 5);
  assert!(
    (120..=135).contains(&r),
    "expected ~50% blended red at 1000ms, got rgba=({r},{g},{b},{a})"
  );
  assert_eq!((g, b, a), (0, 0, 255));

  // At the end boundary (delay + duration), the transition should stop applying and the element
  // should render at its after-change computed value (opacity 1).
  let pixmap_end = prepared
    .paint_with_options(
      PreparedPaintOptions::new()
        .with_background(bg)
        .with_animation_time(1500.0),
    )
    .expect("paint end");
  assert_eq!(pixel(&pixmap_end, 5, 5), (255, 0, 0, 255));
}
