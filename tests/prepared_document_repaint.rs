use fastrender::scroll::ScrollState;
use fastrender::style::types::Overflow;
use fastrender::{
  FastRender, FragmentContent, FragmentNode, Point, PreparedPaintOptions, RenderOptions, Result,
};

fn pixel(pixmap: &fastrender::Pixmap, x: u32, y: u32) -> (u8, u8, u8, u8) {
  let px = pixmap.pixel(x, y).unwrap();
  (px.red(), px.green(), px.blue(), px.alpha())
}

fn fragment_box_id(node: &FragmentNode) -> Option<usize> {
  match &node.content {
    FragmentContent::Block { box_id }
    | FragmentContent::Inline { box_id, .. }
    | FragmentContent::Text { box_id, .. }
    | FragmentContent::Replaced { box_id, .. } => *box_id,
    FragmentContent::RunningAnchor { .. } | FragmentContent::Line { .. } => None,
  }
}

fn find_scroll_container_id(node: &FragmentNode) -> Option<usize> {
  let is_scroll_container = node
    .style
    .as_ref()
    .map(|style| {
      matches!(style.overflow_x, Overflow::Scroll | Overflow::Auto)
        || matches!(style.overflow_y, Overflow::Scroll | Overflow::Auto)
    })
    .unwrap_or(false);
  if is_scroll_container {
    if let Some(id) = fragment_box_id(node) {
      return Some(id);
    }
  }
  for child in node.children.iter() {
    if let Some(id) = find_scroll_container_id(child) {
      return Some(id);
    }
  }
  None
}

#[test]
fn repaint_with_different_scroll_offsets_changes_pixels() -> Result<()> {
  let mut renderer = FastRender::new()?;
  let html = r#"
    <style>
      body { margin: 0; }
      .section { width: 100px; height: 100px; }
      .top { background: rgb(255, 0, 0); }
      .bottom { background: rgb(0, 0, 255); }
    </style>
    <div class="section top"></div>
    <div class="section bottom"></div>
  "#;
  let prepared = renderer.prepare_html(html, RenderOptions::new().with_viewport(100, 100))?;

  let top_first = prepared.paint_with_options(PreparedPaintOptions::new().with_scroll(0.0, 0.0))?;
  let bottom = prepared.paint_with_options(PreparedPaintOptions::new().with_scroll(0.0, 100.0))?;
  let top_second =
    prepared.paint_with_options(PreparedPaintOptions::new().with_scroll(0.0, 0.0))?;

  assert_ne!(pixel(&top_first, 50, 50), pixel(&bottom, 50, 50));
  assert_eq!(top_first.data(), top_second.data());
  Ok(())
}

#[test]
fn repaint_with_different_animation_times_changes_pixels() -> Result<()> {
  let mut renderer = FastRender::new()?;
  let html = r#"
    <style>
      body { margin: 0; }
      /* Note: `animation-duration` defaults to 0s, so we must set a non-zero duration
         to ensure time-based sampling affects the painted output. */
      .box {
        width: 100px;
        height: 100px;
        animation-name: fade;
        animation-duration: 1000ms;
        animation-timing-function: linear;
      }
      @keyframes fade {
        from { background-color: rgb(255, 0, 0); }
        to { background-color: rgb(0, 255, 0); }
      }
    </style>
    <div class="box"></div>
  "#;
  let prepared = renderer.prepare_html(html, RenderOptions::new().with_viewport(100, 100))?;

  let early = prepared.paint_with_options(PreparedPaintOptions::new().with_animation_time(0.0))?;
  let later =
    prepared.paint_with_options(PreparedPaintOptions::new().with_animation_time(800.0))?;
  let repeat = prepared.paint_with_options(PreparedPaintOptions::new().with_animation_time(0.0))?;

  assert_ne!(pixel(&early, 50, 50), pixel(&later, 50, 50));
  assert_eq!(early.data(), repeat.data());
  Ok(())
}

#[test]
fn repaint_with_animation_composition_add_combines_transforms() -> Result<()> {
  let mut renderer = FastRender::new()?;
  let html = r#"
    <style>
      html, body { margin: 0; background: rgb(255, 255, 255); }
      .box {
        width: 50px;
        height: 50px;
        background: rgb(255, 0, 0);
        animation-name: move-x, move-y;
        animation-duration: 1000ms, 1000ms;
        animation-timing-function: linear, linear;
        animation-composition: add;
      }
      @keyframes move-x {
        from { transform: translateX(0px); }
        to { transform: translateX(100px); }
      }
      @keyframes move-y {
        from { transform: translateY(0px); }
        to { transform: translateY(100px); }
      }
    </style>
    <div class="box"></div>
  "#;
  let prepared = renderer.prepare_html(html, RenderOptions::new().with_viewport(150, 150))?;
  let mid = prepared.paint_with_options(PreparedPaintOptions::new().with_animation_time(500.0))?;

  assert_eq!(pixel(&mid, 75, 75), (255, 0, 0, 255));
  assert_eq!(pixel(&mid, 25, 75), (255, 255, 255, 255));
  Ok(())
}

#[test]
fn repaint_with_animation_composition_add_preserves_transform_order() -> Result<()> {
  let mut renderer = FastRender::new()?;
  let html = r#"
    <style>
      html, body { margin: 0; background: rgb(255, 255, 255); }
      .box {
        width: 20px;
        height: 20px;
        background: rgb(255, 0, 0);
        transform-origin: 0 0;
        transform: translateX(100px);
        animation-name: spin;
        animation-duration: 1000ms;
        animation-timing-function: linear;
        animation-composition: add;
      }
      @keyframes spin {
        from { transform: rotate(0deg); }
        to { transform: rotate(90deg); }
      }
    </style>
    <div class="box"></div>
  "#;
  let prepared = renderer.prepare_html(html, RenderOptions::new().with_viewport(200, 200))?;
  let mid = prepared.paint_with_options(PreparedPaintOptions::new().with_animation_time(500.0))?;

  // The base translateX should not be rotated by the additive rotation.
  assert_eq!(pixel(&mid, 100, 15), (255, 0, 0, 255));
  assert_eq!(pixel(&mid, 70, 80), (255, 255, 255, 255));
  Ok(())
}

#[test]
fn repaint_with_animation_composition_add_combines_translate() -> Result<()> {
  let mut renderer = FastRender::new()?;
  let html = r#"
    <style>
      html, body { margin: 0; background: rgb(255, 255, 255); }
      .box {
        width: 50px;
        height: 50px;
        background: rgb(255, 0, 0);
        animation-name: move-x, move-y;
        animation-duration: 1000ms, 1000ms;
        animation-timing-function: linear, linear;
        animation-composition: add;
      }
      @keyframes move-x {
        from { translate: 0px 0px; }
        to { translate: 100px 0px; }
      }
      @keyframes move-y {
        from { translate: 0px 0px; }
        to { translate: 0px 100px; }
      }
    </style>
    <div class="box"></div>
  "#;
  let prepared = renderer.prepare_html(html, RenderOptions::new().with_viewport(150, 150))?;
  let mid = prepared.paint_with_options(PreparedPaintOptions::new().with_animation_time(500.0))?;

  assert_eq!(pixel(&mid, 75, 75), (255, 0, 0, 255));
  assert_eq!(pixel(&mid, 25, 75), (255, 255, 255, 255));
  Ok(())
}

#[test]
fn repaint_with_animation_composition_add_combines_rotate() -> Result<()> {
  let mut renderer = FastRender::new()?;
  let html = r#"
    <style>
      html, body { margin: 0; background: rgb(255, 255, 255); }
      .box {
        position: absolute;
        left: 50px;
        top: 50px;
        width: 50px;
        height: 50px;
        background: rgb(255, 0, 0);
        transform-origin: 0 0;
        animation-name: r1, r2;
        animation-duration: 1000ms, 1000ms;
        animation-timing-function: linear, linear;
        animation-fill-mode: forwards, forwards;
        animation-composition: add;
      }
      .marker {
        position: absolute;
        left: 10px;
        top: 10px;
        width: 10px;
        height: 10px;
        background: rgb(0, 0, 255);
      }
      @keyframes r1 { to { rotate: 90deg; } }
      @keyframes r2 { to { rotate: 90deg; } }
    </style>
    <div class="box"><div class="marker"></div></div>
  "#;
  let prepared = renderer.prepare_html(html, RenderOptions::new().with_viewport(100, 100))?;
  let end = prepared.paint_with_options(PreparedPaintOptions::new().with_animation_time(1000.0))?;

  // Two 90deg rotations should add to a 180deg rotation. With `transform-origin: 0 0` and the box
  // positioned at (50px,50px), the marker should rotate into the top-left quadrant (a pixel like
  // (35px,35px) becomes blue).
  //
  // If composition was incorrectly treated as replace, a single 90deg rotation would move the
  // marker into the lower half of the viewport instead.
  assert_eq!(pixel(&end, 35, 35), (0, 0, 255, 255));
  assert_eq!(pixel(&end, 35, 65), (255, 255, 255, 255));
  assert_eq!(pixel(&end, 65, 65), (255, 255, 255, 255));
  Ok(())
}

#[test]
fn repaint_with_animation_composition_add_combines_scale() -> Result<()> {
  let mut renderer = FastRender::new()?;
  let html = r#"
    <style>
      html, body { margin: 0; background: rgb(255, 255, 255); }
      .box {
        width: 10px;
        height: 10px;
        background: rgb(255, 0, 0);
        transform-origin: 0 0;
        animation-name: s1, s2;
        animation-duration: 1000ms, 1000ms;
        animation-timing-function: linear, linear;
        animation-fill-mode: forwards, forwards;
        animation-composition: add;
      }
      @keyframes s1 { to { scale: 2; } }
      @keyframes s2 { to { scale: 2; } }
    </style>
    <div class="box"></div>
  "#;
  let prepared = renderer.prepare_html(html, RenderOptions::new().with_viewport(100, 100))?;
  let end = prepared.paint_with_options(PreparedPaintOptions::new().with_animation_time(1000.0))?;

  // Two scale(2) effects should multiply to scale(4), expanding the box to 40x40.
  assert_eq!(pixel(&end, 30, 5), (255, 0, 0, 255));
  assert_eq!(pixel(&end, 50, 5), (255, 255, 255, 255));
  Ok(())
}

#[test]
fn repaint_with_animation_composition_accumulate_accumulates_translate_iterations() -> Result<()> {
  let mut renderer = FastRender::new()?;
  let html = r#"
    <style>
      html, body { margin: 0; background: rgb(255, 255, 255); }
      .box {
        width: 10px;
        height: 10px;
        background: rgb(255, 0, 0);
        animation-name: move;
        animation-duration: 1000ms;
        animation-timing-function: linear;
        animation-iteration-count: 2;
        animation-fill-mode: forwards;
        animation-composition: accumulate;
      }
      @keyframes move {
        from { translate: 0px 0px; }
        to { translate: 20px 0px; }
      }
    </style>
    <div class="box"></div>
  "#;
  let prepared = renderer.prepare_html(html, RenderOptions::new().with_viewport(80, 20))?;
  let mid_second =
    prepared.paint_with_options(PreparedPaintOptions::new().with_animation_time(1500.0))?;

  // With `accumulate`, the second iteration continues from the end of the first one.
  // At t=1500ms we are halfway through the second iteration: 20px (first iteration) + 10px = 30px.
  assert_eq!(pixel(&mid_second, 35, 5), (255, 0, 0, 255));
  assert_eq!(pixel(&mid_second, 15, 5), (255, 255, 255, 255));
  Ok(())
}

#[test]
fn repaint_with_animation_composition_accumulate_accumulates_scale_iterations() -> Result<()> {
  let mut renderer = FastRender::new()?;
  let html = r#"
    <style>
      html, body { margin: 0; background: rgb(255, 255, 255); }
      .box {
        width: 10px;
        height: 10px;
        background: rgb(255, 0, 0);
        transform-origin: 0 0;
        animation-name: grow;
        animation-duration: 1000ms;
        animation-timing-function: linear;
        animation-iteration-count: 2;
        animation-fill-mode: forwards;
        animation-composition: accumulate;
      }
      @keyframes grow {
        from { scale: 1; }
        to { scale: 2; }
      }
    </style>
    <div class="box"></div>
  "#;
  let prepared = renderer.prepare_html(html, RenderOptions::new().with_viewport(60, 20))?;
  let end = prepared.paint_with_options(PreparedPaintOptions::new().with_animation_time(2000.0))?;

  // Each iteration scales by a factor of 2. After two iterations the box should be scaled by 4,
  // expanding to 40x40.
  assert_eq!(pixel(&end, 30, 5), (255, 0, 0, 255));
  assert_eq!(pixel(&end, 45, 5), (255, 255, 255, 255));
  Ok(())
}

#[test]
fn repaint_with_animation_delay_and_fill_mode_changes_pixels() -> Result<()> {
  let mut renderer = FastRender::new()?;
  let html = r#"
    <style>
      body { margin: 0; }
      /* Note: `animation-duration` defaults to 0s; set a non-zero duration so that
         the animation timeline meaningfully affects sampled pixels. */
      .box {
        width: 100px;
        height: 100px;
        background-color: rgb(0, 0, 255);
        animation-name: fade;
        animation-duration: 1000ms;
        animation-delay: 500ms;
        animation-fill-mode: forwards;
        animation-timing-function: linear;
      }
      @keyframes fade {
        from { background-color: rgb(255, 0, 0); }
        to { background-color: rgb(0, 255, 0); }
      }
    </style>
    <div class="box"></div>
  "#;
  let prepared = renderer.prepare_html(html, RenderOptions::new().with_viewport(100, 100))?;

  // With a positive delay and no backwards fill-mode, the animation has no effect
  // before the delay elapses.
  let before_delay =
    prepared.paint_with_options(PreparedPaintOptions::new().with_animation_time(0.0))?;
  // With `fill-mode: forwards`, once the active duration elapses the end keyframe
  // should keep applying even after the animation ends.
  let after_end =
    prepared.paint_with_options(PreparedPaintOptions::new().with_animation_time(1600.0))?;
  let before_delay_repeat =
    prepared.paint_with_options(PreparedPaintOptions::new().with_animation_time(0.0))?;
  let after_end_repeat =
    prepared.paint_with_options(PreparedPaintOptions::new().with_animation_time(1600.0))?;

  assert_eq!(pixel(&before_delay, 50, 50), (0, 0, 255, 255));
  assert_eq!(pixel(&after_end, 50, 50), (0, 255, 0, 255));
  assert_ne!(pixel(&before_delay, 50, 50), pixel(&after_end, 50, 50));
  assert_eq!(before_delay.data(), before_delay_repeat.data());
  assert_eq!(after_end.data(), after_end_repeat.data());
  Ok(())
}

#[test]
fn repaint_with_element_scroll_offsets_changes_pixels() -> Result<()> {
  let mut renderer = FastRender::new()?;
  let html = r#"
    <style>
      body { margin: 0; }
      .scroller { width: 100px; height: 100px; overflow: scroll; }
      .section { width: 100px; height: 100px; }
      .top { background: rgb(255, 0, 0); }
      .bottom { background: rgb(0, 0, 255); }
    </style>
    <div class="scroller">
      <div class="section top"></div>
      <div class="section bottom"></div>
    </div>
  "#;
  let prepared = renderer.prepare_html(html, RenderOptions::new().with_viewport(100, 100))?;

  let scroller_id = find_scroll_container_id(&prepared.fragment_tree().root)
    .expect("scroll container fragment with overflow");

  let base = prepared.paint_with_options(
    PreparedPaintOptions::new().with_scroll_state(ScrollState::with_viewport(Point::ZERO)),
  )?;

  let mut scrolled_state = ScrollState::with_viewport(Point::ZERO);
  scrolled_state
    .elements
    .insert(scroller_id, Point::new(0.0, 100.0));
  let scrolled = prepared
    .paint_with_options(PreparedPaintOptions::new().with_scroll_state(scrolled_state.clone()))?;
  let scrolled_repeat =
    prepared.paint_with_options(PreparedPaintOptions::new().with_scroll_state(scrolled_state))?;
  let base_repeat = prepared.paint_with_options(
    PreparedPaintOptions::new().with_scroll_state(ScrollState::with_viewport(Point::ZERO)),
  )?;

  assert_ne!(pixel(&base, 50, 50), pixel(&scrolled, 50, 50));
  assert_eq!(base.data(), base_repeat.data());
  assert_eq!(scrolled.data(), scrolled_repeat.data());
  Ok(())
}

#[test]
fn repaint_with_view_timeline_range_changes_pixels() -> Result<()> {
  let mut renderer = FastRender::new()?;
  let html = r#"
    <style>
      body { margin: 0; }
      .spacer { height: 200px; }
      .box {
        width: 200px;
        height: 100px;
        view-timeline: --box block;
        animation-name: fade;
        animation-timeline: --box;
        animation-range: entry 50px entry 150px;
      }
      @keyframes fade {
        from { background-color: rgb(255, 0, 0); }
        to { background-color: rgb(0, 255, 0); }
      }
    </style>
    <div class="spacer"></div>
    <div class="box"></div>
    <div class="spacer" style="height: 800px"></div>
  "#;
  let prepared = renderer.prepare_html(html, RenderOptions::new().with_viewport(200, 200))?;

  // The box starts at y=200, so its entry phase begins at scroll_y=0 for a 200px viewport.
  // With `animation-range: entry 50px entry 150px`, scroll_y=50 should be progress 0 and
  // scroll_y=150 should be progress 1.
  let start = prepared.paint_with_options(PreparedPaintOptions::new().with_scroll(0.0, 50.0))?;
  let mid = prepared.paint_with_options(PreparedPaintOptions::new().with_scroll(0.0, 100.0))?;
  let end = prepared.paint_with_options(PreparedPaintOptions::new().with_scroll(0.0, 150.0))?;

  let start_px = pixel(&start, 10, 160);
  let mid_px = pixel(&mid, 10, 110);
  let end_px = pixel(&end, 10, 60);

  assert_eq!(start_px, (255, 0, 0, 255));
  assert_eq!(end_px, (0, 255, 0, 255));
  assert_ne!(mid_px, start_px);
  assert_ne!(mid_px, end_px);
  Ok(())
}

#[test]
fn repaint_with_view_timeline_range_longhands_changes_pixels() -> Result<()> {
  let mut renderer = FastRender::new()?;
  let html = r#"
    <style>
      body { margin: 0; }
      .spacer { height: 200px; }
      .box {
        width: 200px;
        height: 100px;
        view-timeline: --box block;
        animation-name: fade;
        animation-timeline: --box;
        animation-range-start: entry 50px;
        animation-range-end: entry 150px;
      }
      @keyframes fade {
        from { background-color: rgb(255, 0, 0); }
        to { background-color: rgb(0, 255, 0); }
      }
    </style>
    <div class="spacer"></div>
    <div class="box"></div>
    <div class="spacer" style="height: 800px"></div>
  "#;
  let prepared = renderer.prepare_html(html, RenderOptions::new().with_viewport(200, 200))?;

  // The box starts at y=200, so its entry phase begins at scroll_y=0 for a 200px viewport.
  // With `animation-range-start/end: entry 50px` / `entry 150px`, scroll_y=50 should be progress 0 and
  // scroll_y=150 should be progress 1.
  let start = prepared.paint_with_options(PreparedPaintOptions::new().with_scroll(0.0, 50.0))?;
  let mid = prepared.paint_with_options(PreparedPaintOptions::new().with_scroll(0.0, 100.0))?;
  let end = prepared.paint_with_options(PreparedPaintOptions::new().with_scroll(0.0, 150.0))?;

  let start_px = pixel(&start, 10, 160);
  let mid_px = pixel(&mid, 10, 110);
  let end_px = pixel(&end, 10, 60);

  assert_eq!(start_px, (255, 0, 0, 255));
  assert_eq!(end_px, (0, 255, 0, 255));
  assert_ne!(mid_px, start_px);
  assert_ne!(mid_px, end_px);
  Ok(())
}
