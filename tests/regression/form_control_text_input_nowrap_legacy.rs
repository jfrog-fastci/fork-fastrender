use fastrender::debug::runtime::RuntimeToggles;
use fastrender::resource::ResourcePolicy;
use fastrender::scroll::ScrollState;
use fastrender::{BoxType, FastRender, FontConfig, LayoutParallelism, PaintParallelism, Point, Rgba, RenderOptions};
use std::collections::HashMap;
use std::sync::Once;

static INIT: Once = Once::new();

fn ensure_test_env() {
  INIT.call_once(|| {
    // See `tests/animation/support.rs` for background.
    crate::common::init_rayon_for_tests(1);
  });
}

fn bbox_for_ink(pixmap: &fastrender::Pixmap) -> Option<(u32, u32, u32, u32)> {
  let mut min_x = u32::MAX;
  let mut min_y = u32::MAX;
  let mut max_x = 0u32;
  let mut max_y = 0u32;

  for y in 0..pixmap.height() {
    for x in 0..pixmap.width() {
      let px = pixmap.pixel(x, y).expect("pixel in bounds");
      // We render on an opaque white background; treat any non-white pixel as text ink.
      if px.alpha() != 0 && (px.red() < 250 || px.green() < 250 || px.blue() < 250) {
        min_x = min_x.min(x);
        min_y = min_y.min(y);
        max_x = max_x.max(x);
        max_y = max_y.max(y);
      }
    }
  }

  (min_x != u32::MAX).then_some((min_x, min_y, max_x, max_y))
}

fn ink_row_counts(pixmap: &fastrender::Pixmap) -> Vec<u32> {
  let mut counts = vec![0u32; pixmap.height() as usize];
  for y in 0..pixmap.height() {
    let mut count = 0u32;
    for x in 0..pixmap.width() {
      let px = pixmap.pixel(x, y).expect("pixel in bounds");
      if px.alpha() != 0 && (px.red() < 250 || px.green() < 250 || px.blue() < 250) {
        count += 1;
      }
    }
    counts[y as usize] = count;
  }
  counts
}

fn count_vertical_bands(row_counts: &[u32], min_row_ink: u32) -> u32 {
  let mut bands = 0u32;
  let mut in_band = false;
  for &count in row_counts {
    let has_ink = count >= min_row_ink;
    if has_ink && !in_band {
      bands += 1;
      in_band = true;
    } else if !has_ink {
      in_band = false;
    }
  }
  bands
}

fn first_form_control_box_id(root: &fastrender::BoxNode) -> Option<usize> {
  let mut stack: Vec<&fastrender::BoxNode> = vec![root];
  while let Some(node) = stack.pop() {
    if let BoxType::Replaced(replaced) = &node.box_type {
      if matches!(
        replaced.replaced_type,
        fastrender::tree::box_tree::ReplacedType::FormControl(_)
      ) {
        return Some(node.id);
      }
    }
    if let Some(body) = node.footnote_body.as_deref() {
      stack.push(body);
    }
    for child in node.children.iter().rev() {
      stack.push(child);
    }
  }
  None
}

#[test]
fn legacy_text_input_forces_single_line_value_no_wrap() {
  ensure_test_env();

  // Force the legacy paint backend so this test exercises the code path in `src/paint/painter.rs`
  // (display-list painting already forces nowrap for `<input>` text values).
  let toggles = RuntimeToggles::from_map(HashMap::from([(
    "FASTR_PAINT_BACKEND".to_string(),
    "legacy".to_string(),
  )]));

  let mut renderer = FastRender::builder()
    .font_sources(FontConfig::bundled_only())
    .runtime_toggles(toggles.clone())
    .resource_policy(
      ResourcePolicy::default()
        .allow_http(false)
        .allow_https(false),
    )
    .paint_parallelism(PaintParallelism::disabled())
    .build()
    .expect("renderer");

  let html = r#"
    <!doctype html>
    <style>
      html, body { margin: 0; background: white; }
      input {
        width: 30px;
        height: 80px;
        padding: 0;
        border: none;
        background: transparent;
        font-family: "Noto Sans", sans-serif;
        /* Ensure author styles would normally allow wrapping in a multi-line context. */
        white-space: pre-wrap;
        text-wrap: wrap;
        font-size: 16px;
        color: black;
      }
    </style>
    <input type="text" value="MMMM MMMM MMMM MMMM MMMM MMMM MMMM MMMM" />
  "#;

  let options = RenderOptions::new()
    .with_viewport(120, 120)
    .with_runtime_toggles(toggles)
    .with_layout_parallelism(LayoutParallelism::disabled());
  let pixmap = renderer
    .render_html_with_options(html, options)
    .expect("render");

  let (_, min_y, _, max_y) = bbox_for_ink(&pixmap).expect("expected input value text to paint ink");
  let ink_height = max_y - min_y + 1;
  let row_counts = ink_row_counts(&pixmap);
  // Require a small number of pixels so antialias noise doesn't count as a band.
  let ink_bands = count_vertical_bands(&row_counts, 5);

  // With the legacy wrapping bug, the long value would soft-wrap into multiple lines because the
  // input is narrow but tall enough to fit them. We expect a single line of text, so the vertical
  // ink height should stay well below two line boxes.
  assert_eq!(
    ink_bands, 1,
    "expected `<input>` value to paint on a single band in legacy backend; bands={ink_bands}"
  );
  assert!(
    ink_height < 30,
    "expected `<input>` value to paint as a single line in legacy backend; ink height={ink_height} (y={min_y}..={max_y})"
  );
}

#[test]
fn legacy_text_input_forces_single_line_placeholder_no_wrap() {
  ensure_test_env();

  let toggles = RuntimeToggles::from_map(HashMap::from([(
    "FASTR_PAINT_BACKEND".to_string(),
    "legacy".to_string(),
  )]));

  let mut renderer = FastRender::builder()
    .font_sources(FontConfig::bundled_only())
    .runtime_toggles(toggles.clone())
    .resource_policy(
      ResourcePolicy::default()
        .allow_http(false)
        .allow_https(false),
    )
    .paint_parallelism(PaintParallelism::disabled())
    .build()
    .expect("renderer");

  let html = r#"
    <!doctype html>
    <style>
      html, body { margin: 0; background: white; }
      input {
        width: 30px;
        height: 80px;
        padding: 0;
        border: none;
        background: transparent;
        font-family: "Noto Sans", sans-serif;
        font-size: 16px;
        color: black;
      }
      /* Ensure placeholder pseudo styles would normally allow wrapping. */
      input::placeholder {
        white-space: pre-wrap;
        text-wrap: wrap;
        color: black;
        opacity: 1;
      }
    </style>
    <input type="text" placeholder="MMMM MMMM MMMM MMMM MMMM MMMM MMMM MMMM" />
  "#;

  let options = RenderOptions::new()
    .with_viewport(120, 120)
    .with_runtime_toggles(toggles)
    .with_layout_parallelism(LayoutParallelism::disabled());
  let pixmap = renderer
    .render_html_with_options(html, options)
    .expect("render");

  let (_, min_y, _, max_y) =
    bbox_for_ink(&pixmap).expect("expected input placeholder text to paint ink");
  let ink_height = max_y - min_y + 1;
  let row_counts = ink_row_counts(&pixmap);
  let ink_bands = count_vertical_bands(&row_counts, 5);

  assert_eq!(
    ink_bands, 1,
    "expected `<input>` placeholder to paint on a single band in legacy backend; bands={ink_bands}"
  );
  assert!(
    ink_height < 30,
    "expected `<input>` placeholder to paint as a single line in legacy backend; ink height={ink_height} (y={min_y}..={max_y})"
  );
}

#[test]
fn legacy_text_input_scroll_x_is_clamped_when_focused() {
  ensure_test_env();

  let toggles = RuntimeToggles::from_map(HashMap::from([(
    "FASTR_PAINT_BACKEND".to_string(),
    "legacy".to_string(),
  )]));

  let mut renderer = FastRender::builder()
    .font_sources(FontConfig::bundled_only())
    .runtime_toggles(toggles.clone())
    .resource_policy(
      ResourcePolicy::default()
        .allow_http(false)
        .allow_https(false),
    )
    .paint_parallelism(PaintParallelism::disabled())
    .layout_parallelism(LayoutParallelism::disabled())
    .build()
    .expect("renderer");

  // Use autofocus so the input is focused and we exercise the scroll_x clamping logic (which lives
  // on the focused path).
  let html = r#"
    <!doctype html>
    <style>
      html, body { margin: 0; background: white; }
      input {
        width: 30px;
        height: 80px;
        padding: 0;
        border: none;
        background: transparent;
        font-family: "Noto Sans", sans-serif;
        /* Avoid caret ink so the test detects whether the text itself is visible after clamping. */
        caret-color: transparent;
        /* Wrapping-friendly authored styles; the painter must still force nowrap. */
        white-space: pre-wrap;
        text-wrap: wrap;
        font-size: 16px;
        color: black;
      }
    </style>
    <input type="text" autofocus value="MMMM MMMM MMMM MMMM MMMM MMMM MMMM MMMM" />
  "#;

  let options = RenderOptions::new()
    .with_viewport(120, 120)
    .with_runtime_toggles(toggles);
  let prepared = renderer.prepare_html(html, options).expect("prepare html");

  let input_box_id =
    first_form_control_box_id(&prepared.box_tree().root).expect("find input box_id");

  // Provide an absurd scroll offset; the paint path should clamp it to the max scroll range rather
  // than scrolling the text completely out of view.
  let mut element_offsets = HashMap::new();
  element_offsets.insert(input_box_id, Point::new(10_000.0, 0.0));
  let scroll_state = ScrollState::from_parts(Point::ZERO, element_offsets);

  let pixmap = prepared
    .paint_with_scroll_state(scroll_state, Some((120, 120)), Some(Rgba::WHITE), None)
    .expect("paint");

  let row_counts = ink_row_counts(&pixmap);
  let ink_bands = count_vertical_bands(&row_counts, 5);
  assert_eq!(
    ink_bands, 1,
    "expected scrolled `<input>` value to still paint as a single line; bands={ink_bands}"
  );
}

#[test]
fn legacy_password_input_forces_single_line_mask_no_wrap() {
  ensure_test_env();

  let toggles = RuntimeToggles::from_map(HashMap::from([(
    "FASTR_PAINT_BACKEND".to_string(),
    "legacy".to_string(),
  )]));

  let mut renderer = FastRender::builder()
    .font_sources(FontConfig::bundled_only())
    .runtime_toggles(toggles.clone())
    .resource_policy(
      ResourcePolicy::default()
        .allow_http(false)
        .allow_https(false),
    )
    .paint_parallelism(PaintParallelism::disabled())
    .layout_parallelism(LayoutParallelism::disabled())
    .build()
    .expect("renderer");

  let html = r#"
    <!doctype html>
    <style>
      html, body { margin: 0; background: white; }
      input {
        width: 30px;
        height: 80px;
        padding: 0;
        border: none;
        background: transparent;
        font-family: "Noto Sans", sans-serif;
        /* Wrapping-friendly authored styles; the painter must still force nowrap. */
        white-space: pre-wrap;
        text-wrap: wrap;
        font-size: 16px;
        color: black;
      }
    </style>
    <input type=\"password\" value=\"this-is-a-long-password-value\" />
  "#;

  let options = RenderOptions::new()
    .with_viewport(120, 120)
    .with_layout_parallelism(LayoutParallelism::disabled())
    .with_runtime_toggles(toggles);
  let pixmap = renderer
    .render_html_with_options(html, options)
    .expect("render");

  let (_, min_y, _, max_y) = bbox_for_ink(&pixmap).expect("expected password text to paint ink");
  let ink_height = max_y - min_y + 1;
  let row_counts = ink_row_counts(&pixmap);
  let ink_bands = count_vertical_bands(&row_counts, 5);

  assert_eq!(
    ink_bands, 1,
    "expected `<input type=password>` mask to paint on a single band in legacy backend; bands={ink_bands}"
  );
  assert!(
    ink_height < 30,
    "expected `<input type=password>` mask to paint as a single line in legacy backend; ink height={ink_height} (y={min_y}..={max_y})"
  );
}
