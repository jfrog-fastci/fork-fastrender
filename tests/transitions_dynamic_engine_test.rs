use fastrender::animation;
use fastrender::interaction::dom_index::DomIndex;
use fastrender::interaction::dom_mutation;
use fastrender::style::cascade::StyledNode;
use fastrender::style::color::Rgba;
use fastrender::style::types::BorderStyle;
use fastrender::style::values::CustomPropertyTypedValue;
use fastrender::tree::box_tree::{BoxNode, GeneratedPseudoElement};
use fastrender::tree::fragment_tree::{FragmentNode, FragmentTree};
use fastrender::{BrowserDocument, PreparedDocument, RenderOptions, Result};
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

fn styled_node_id_by_id(styled: &StyledNode, target_id: &str) -> Option<usize> {
  if styled
    .node
    .get_attribute("id")
    .is_some_and(|id| id.eq_ignore_ascii_case(target_id))
  {
    return Some(styled.node_id);
  }
  for child in &styled.children {
    if let Some(id) = styled_node_id_by_id(child, target_id) {
      return Some(id);
    }
  }
  None
}

fn box_id_for_styled(box_node: &BoxNode, styled_id: usize) -> Option<usize> {
  box_id_for_styled_and_pseudo(box_node, styled_id, None)
}

fn box_id_for_styled_and_pseudo(
  box_node: &BoxNode,
  styled_id: usize,
  pseudo: Option<GeneratedPseudoElement>,
) -> Option<usize> {
  if box_node.styled_node_id == Some(styled_id) && box_node.generated_pseudo == pseudo {
    return Some(box_node.id);
  }
  for child in &box_node.children {
    if let Some(id) = box_id_for_styled_and_pseudo(child, styled_id, pseudo) {
      return Some(id);
    }
  }
  if let Some(body) = box_node.footnote_body.as_deref() {
    if let Some(id) = box_id_for_styled_and_pseudo(body, styled_id, pseudo) {
      return Some(id);
    }
  }
  None
}

fn find_fragment<'a>(fragment: &'a FragmentNode, box_id: usize) -> Option<&'a FragmentNode> {
  if fragment.box_id() == Some(box_id) {
    return Some(fragment);
  }
  for child in fragment.children.iter() {
    if let Some(found) = find_fragment(child, box_id) {
      return Some(found);
    }
  }
  if let FragmentNode {
    content: fastrender::tree::fragment_tree::FragmentContent::RunningAnchor { snapshot, .. },
    ..
  } = fragment
  {
    return find_fragment(snapshot, box_id);
  }
  None
}

fn fragment_opacity(tree: &FragmentTree, box_id: usize) -> f32 {
  let frag = find_fragment(&tree.root, box_id).expect("fragment present");
  frag
    .style
    .as_ref()
    .map(|s| s.opacity)
    .expect("style present")
}

fn fragment_color(tree: &FragmentTree, box_id: usize) -> Rgba {
  let frag = find_fragment(&tree.root, box_id).expect("fragment present");
  frag
    .style
    .as_ref()
    .map(|s| s.color)
    .expect("style present")
}

fn fragment_border_top_style(tree: &FragmentTree, box_id: usize) -> BorderStyle {
  let frag = find_fragment(&tree.root, box_id).expect("fragment present");
  let style = frag.style.as_ref().expect("style present");
  style.border_top_style
}

fn fragment_border_top_color(tree: &FragmentTree, box_id: usize) -> Rgba {
  let frag = find_fragment(&tree.root, box_id).expect("fragment present");
  frag
    .style
    .as_ref()
    .map(|s| s.border_top_color)
    .expect("style present")
}

fn box_id_by_element_id(prepared: &PreparedDocument, element_id: &str) -> usize {
  let styled_id = styled_node_id_by_id(prepared.styled_tree(), element_id).expect("styled id");
  box_id_for_styled(&prepared.box_tree().root, styled_id).expect("box id")
}

fn set_class(doc: &mut BrowserDocument, element_id: &str, class: &str) -> bool {
  doc.mutate_dom(|dom| {
    let mut index = DomIndex::build(dom);
    let node_id = *index
      .id_by_element_id
      .get(element_id)
      .unwrap_or_else(|| panic!("expected #{element_id} element"));
    index
      .with_node_mut(node_id, |node| dom_mutation::set_attr(node, "class", class))
      .unwrap_or(false)
  })
}

fn set_attr(doc: &mut BrowserDocument, element_id: &str, name: &str, value: &str) -> bool {
  doc.mutate_dom(|dom| {
    let mut index = DomIndex::build(dom);
    let node_id = *index
      .id_by_element_id
      .get(element_id)
      .unwrap_or_else(|| panic!("expected #{element_id} element"));
    index
      .with_node_mut(node_id, |node| dom_mutation::set_attr(node, name, value))
      .unwrap_or(false)
  })
}

fn remove_attr(doc: &mut BrowserDocument, element_id: &str, name: &str) -> bool {
  doc.mutate_dom(|dom| {
    let mut index = DomIndex::build(dom);
    let node_id = *index
      .id_by_element_id
      .get(element_id)
      .unwrap_or_else(|| panic!("expected #{element_id} element"));
    index
      .with_node_mut(node_id, |node| dom_mutation::remove_attr(node, name))
      .unwrap_or(false)
  })
}

#[test]
fn class_flip_triggers_transition_opacity() -> Result<()> {
  ensure_test_env();

  let html = r#"
    <style>
      #box { width: 100px; height: 100px; transition: opacity 1000ms linear; }
      .a { opacity: 0; }
      .b { opacity: 1; }
    </style>
    <div id="box" class="a"></div>
  "#;

  let mut doc = BrowserDocument::from_html(
    html,
    RenderOptions::new()
      .with_viewport(200, 200)
      .with_animation_time(0.0),
  )?;

  // First frame initializes the pipeline and establishes the baseline style.
  doc.render_frame()?;

  assert!(set_class(&mut doc, "box", "b"));
  // Keep time at t=0 so this frame records the transition start time.
  doc.render_frame()?;

  let prepared = doc.prepared().expect("prepared");
  let box_id = box_id_by_element_id(prepared, "box");
  let base_tree = prepared.fragment_tree().clone();

  let cases = [(0.0, 0.0), (500.0, 0.5), (1000.0, 1.0)];
  for (time, expected) in cases {
    let mut sampled = base_tree.clone();
    let viewport = sampled.viewport_size();
    animation::apply_transitions(&mut sampled, time, viewport);
    let opacity = fragment_opacity(&sampled, box_id);
    assert!(
      (opacity - expected).abs() < 1e-3,
      "t={time} expected {expected}, got {opacity}"
    );
  }

  Ok(())
}

#[test]
fn transition_delay_positive_holds_start_value_until_delay_elapses() -> Result<()> {
  ensure_test_env();

  let html = r#"
    <style>
      #box { width: 100px; height: 100px; background: black; opacity: 0; transition: opacity 1000ms linear 500ms; }
      #box.b { opacity: 1; }
    </style>
    <div id="box"></div>
  "#;

  let mut doc = BrowserDocument::from_html(
    html,
    RenderOptions::new()
      .with_viewport(200, 200)
      .with_animation_time(0.0),
  )?;

  doc.render_frame()?;
  assert!(set_class(&mut doc, "box", "b"));
  // Keep time at t=0 so this frame records the transition start time.
  doc.render_frame()?;

  let prepared = doc.prepared().expect("prepared");
  let box_id = box_id_by_element_id(prepared, "box");
  let base_tree = prepared.fragment_tree().clone();

  let eps = 1e-3;
  let cases = [(250.0, 0.0), (750.0, 0.25), (1500.0, 1.0)];
  for (time, expected) in cases {
    let mut sampled = base_tree.clone();
    let viewport = sampled.viewport_size();
    animation::apply_transitions(&mut sampled, time, viewport);
    let opacity = fragment_opacity(&sampled, box_id);
    assert!(
      (opacity - expected).abs() < eps,
      "t={time} expected {expected}, got {opacity}"
    );
  }

  Ok(())
}

#[test]
fn transition_delay_negative_starts_partway_through() -> Result<()> {
  ensure_test_env();

  let html = r#"
    <style>
      #box { width: 100px; height: 100px; background: black; opacity: 0; transition: opacity 1000ms linear -500ms; }
      #box.b { opacity: 1; }
    </style>
    <div id="box"></div>
  "#;

  let mut doc = BrowserDocument::from_html(
    html,
    RenderOptions::new()
      .with_viewport(200, 200)
      .with_animation_time(0.0),
  )?;

  doc.render_frame()?;
  assert!(set_class(&mut doc, "box", "b"));
  // Keep time at t=0 so this frame records the transition start time.
  doc.render_frame()?;

  let prepared = doc.prepared().expect("prepared");
  let box_id = box_id_by_element_id(prepared, "box");
  let base_tree = prepared.fragment_tree().clone();

  let eps = 1e-3;
  let cases = [(0.0, 0.5), (250.0, 0.75), (500.0, 1.0)];
  for (time, expected) in cases {
    let mut sampled = base_tree.clone();
    let viewport = sampled.viewport_size();
    animation::apply_transitions(&mut sampled, time, viewport);
    let opacity = fragment_opacity(&sampled, box_id);
    assert!(
      (opacity - expected).abs() < eps,
      "t={time} expected {expected}, got {opacity}"
    );
  }

  Ok(())
}

#[test]
fn transition_delay_negative_finishes_immediately_when_delay_exceeds_duration() -> Result<()> {
  ensure_test_env();

  let html = r#"
    <style>
      #box { width: 100px; height: 100px; background: black; opacity: 0; transition: opacity 1000ms linear -1500ms; }
      #box.b { opacity: 1; }
    </style>
    <div id="box"></div>
  "#;

  let mut doc = BrowserDocument::from_html(
    html,
    RenderOptions::new()
      .with_viewport(200, 200)
      .with_animation_time(0.0),
  )?;

  doc.render_frame()?;
  assert!(set_class(&mut doc, "box", "b"));
  // Keep time at t=0 so this frame records the transition start time.
  doc.render_frame()?;

  let prepared = doc.prepared().expect("prepared");
  let box_id = box_id_by_element_id(prepared, "box");
  let base_tree = prepared.fragment_tree().clone();

  // The transition is considered to have started 1500ms in the past; since duration is 1000ms,
  // it should already be finished at t=0.
  let eps = 1e-3;
  let cases = [(0.0, 1.0), (100.0, 1.0)];
  for (time, expected) in cases {
    let mut sampled = base_tree.clone();
    let viewport = sampled.viewport_size();
    animation::apply_transitions(&mut sampled, time, viewport);
    let opacity = fragment_opacity(&sampled, box_id);
    assert!(
      (opacity - expected).abs() < eps,
      "t={time} expected {expected}, got {opacity}"
    );
  }

  Ok(())
}

#[test]
fn transition_reverses_with_shortened_duration() -> Result<()> {
  ensure_test_env();

  let html = r#"
    <style>
      #box { width: 100px; height: 100px; transition: opacity 1000ms linear; }
      .a { opacity: 0; }
      .b { opacity: 1; }
    </style>
    <div id="box" class="a"></div>
  "#;

  let mut doc = BrowserDocument::from_html(
    html,
    RenderOptions::new()
      .with_viewport(200, 200)
      .with_animation_time(0.0),
  )?;

  doc.render_frame()?;
  assert!(set_class(&mut doc, "box", "b"));
  doc.render_frame()?; // Start A -> B at t=0.

  // Reverse the transition mid-flight at t=200ms.
  doc.set_animation_time_ms(200.0);
  assert!(set_class(&mut doc, "box", "a"));
  doc.render_frame()?; // Trigger B -> A at t=200ms.

  let prepared = doc.prepared().expect("prepared");
  let box_id = box_id_by_element_id(prepared, "box");
  let base_tree = prepared.fragment_tree().clone();

  // Expected for a 1000ms linear transition reversed at 200ms:
  // - current value at reversal: 0.2
  // - reverse duration shortened to 200ms
  let eps = 1e-3;
  let cases = [(200.0, 0.2), (300.0, 0.1), (400.0, 0.0)];
  for (time, expected) in cases {
    let mut sampled = base_tree.clone();
    let viewport = sampled.viewport_size();
    animation::apply_transitions(&mut sampled, time, viewport);
    let opacity = fragment_opacity(&sampled, box_id);
    assert!(
      (opacity - expected).abs() < eps,
      "t={time} expected {expected}, got {opacity}"
    );
  }

  Ok(())
}

#[test]
fn transition_behavior_allow_discrete_gates_discrete_transitions() -> Result<()> {
  ensure_test_env();

  let html = r#"
    <style>
      #box {
        width: 100px;
        height: 100px;
        border-top-width: 10px;
        transition: border-top-style 1000ms linear;
      }
      .a { border-top-style: solid; }
      .b { border-top-style: dashed; }
    </style>
    <div id="box" class="a"></div>
  "#;

  let mut doc = BrowserDocument::from_html(
    html,
    RenderOptions::new()
      .with_viewport(200, 200)
      .with_animation_time(0.0),
  )?;
  doc.render_frame()?;
  assert!(set_class(&mut doc, "box", "b"));
  doc.render_frame()?;

  let prepared = doc.prepared().expect("prepared");
  let box_id = box_id_by_element_id(prepared, "box");
  let base_tree = prepared.fragment_tree().clone();
  let mut sampled = base_tree.clone();
  let viewport = sampled.viewport_size();
  animation::apply_transitions(&mut sampled, 400.0, viewport);
  assert_eq!(
    fragment_border_top_style(&sampled, box_id),
    BorderStyle::Dashed,
    "discrete transitions should not run without allow-discrete"
  );

  let html_allow = r#"
    <style>
      #box {
        width: 100px;
        height: 100px;
        border-top-width: 10px;
        transition: border-top-style 1000ms linear allow-discrete;
      }
      .a { border-top-style: solid; }
      .b { border-top-style: dashed; }
    </style>
    <div id="box" class="a"></div>
  "#;

  let mut doc = BrowserDocument::from_html(
    html_allow,
    RenderOptions::new()
      .with_viewport(200, 200)
      .with_animation_time(0.0),
  )?;
  doc.render_frame()?;
  assert!(set_class(&mut doc, "box", "b"));
  doc.render_frame()?;

  let prepared = doc.prepared().expect("prepared");
  let box_id = box_id_by_element_id(prepared, "box");
  let base_tree = prepared.fragment_tree().clone();

  let mut early = base_tree.clone();
  let viewport = early.viewport_size();
  animation::apply_transitions(&mut early, 400.0, viewport);
  assert_eq!(fragment_border_top_style(&early, box_id), BorderStyle::Solid);

  let mut late = base_tree.clone();
  let viewport = late.viewport_size();
  animation::apply_transitions(&mut late, 600.0, viewport);
  assert_eq!(fragment_border_top_style(&late, box_id), BorderStyle::Dashed);

  Ok(())
}

#[test]
fn transitions_are_keyed_by_pseudo_element() -> Result<()> {
  ensure_test_env();

  let html = r#"
    <style>
      #box { width: 10px; height: 10px; background: black; opacity: 1; transition: opacity 1000ms linear; }
      #box::before {
        content: "";
        display: block;
        width: 10px; height: 10px;
        background: black;
        opacity: 0;
        transition: opacity 2000ms linear;
      }
      #box.b { opacity: 0; }
      #box.b::before { opacity: 1; }
    </style>
    <div id="box"></div>
  "#;

  let mut doc = BrowserDocument::from_html(
    html,
    RenderOptions::new()
      .with_viewport(32, 32)
      .with_animation_time(0.0),
  )?;
  doc.render_frame()?;
  assert!(set_class(&mut doc, "box", "b"));
  // Render again at t=0 to seed the transition start snapshots.
  doc.render_frame()?;

  let prepared = doc.prepared().expect("prepared");
  let styled_id = styled_node_id_by_id(prepared.styled_tree(), "box").expect("styled id");
  let main_box_id = box_id_for_styled_and_pseudo(&prepared.box_tree().root, styled_id, None)
    .expect("main box id");
  let before_box_id = box_id_for_styled_and_pseudo(
    &prepared.box_tree().root,
    styled_id,
    Some(GeneratedPseudoElement::Before),
  )
  .expect("before box id");

  let mut sampled = prepared.fragment_tree().clone();
  let viewport = sampled.viewport_size();
  animation::apply_transitions(&mut sampled, 500.0, viewport);

  let main_opacity = fragment_opacity(&sampled, main_box_id);
  let before_opacity = fragment_opacity(&sampled, before_box_id);

  let eps = 1e-3;
  assert!(
    (main_opacity - 0.5).abs() < eps,
    "expected main opacity ~0.5 at 500ms of 1000ms transition, got {main_opacity} (before={before_opacity})"
  );
  assert!(
    (before_opacity - 0.25).abs() < eps,
    "expected ::before opacity ~0.25 at 500ms of 2000ms transition, got {before_opacity} (main={main_opacity})"
  );

  Ok(())
}

fn pixel(pixmap: &fastrender::Pixmap, x: u32, y: u32) -> (u8, u8, u8, u8) {
  let px = pixmap.pixel(x, y).unwrap();
  (px.red(), px.green(), px.blue(), px.alpha())
}

fn assert_mid_grey(px: (u8, u8, u8, u8)) {
  let (r, g, b, a) = px;
  assert!(
    (120..=135).contains(&r),
    "expected ~50% blended grey, got rgba=({r},{g},{b},{a})"
  );
  assert_eq!(g, r, "expected grey, got rgba=({r},{g},{b},{a})");
  assert_eq!(b, r, "expected grey, got rgba=({r},{g},{b},{a})");
  assert_eq!(a, 255, "expected opaque output, got rgba=({r},{g},{b},{a})");
}

fn assert_grey_range(px: (u8, u8, u8, u8), range: std::ops::RangeInclusive<u8>) {
  let (r, g, b, a) = px;
  assert!(
    range.contains(&r),
    "expected grey in {range:?}, got rgba=({r},{g},{b},{a})"
  );
  assert_eq!(g, r, "expected grey, got rgba=({r},{g},{b},{a})");
  assert_eq!(b, r, "expected grey, got rgba=({r},{g},{b},{a})");
  assert_eq!(a, 255, "expected opaque output, got rgba=({r},{g},{b},{a})");
}

#[test]
fn removing_transition_property_cancels_running_transition() -> Result<()> {
  ensure_test_env();

  let html = r#"
    <style>
      html, body { margin: 0; background: rgb(255, 255, 255); }
      #box { width: 100px; height: 100px; background: black; opacity: 0; transition: opacity 1000ms linear; }
      #box.b { opacity: 1; }
      /* Same target value, but disable transitions */
      #box.no { opacity: 1; transition-property: none; }
    </style>
    <div id="box"></div>
  "#;

  let mut doc = BrowserDocument::from_html(
    html,
    RenderOptions::new()
      .with_viewport(100, 100)
      .with_animation_time(0.0),
  )?;

  // First frame initializes the pipeline and establishes the baseline style.
  doc.render_frame()?;

  // Start the transition at t=0.
  assert!(set_class(&mut doc, "box", "b"));
  doc.render_frame()?;

  // Sample mid-transition at t=500ms.
  doc.set_animation_time_ms(500.0);
  let mid = doc.render_frame()?;
  assert_mid_grey(pixel(&mid, 50, 50));

  // Disable transitions at the same timestamp. Per CSS Transitions 1, changing
  // `transition-property` such that the transition would not have started cancels the running
  // transition and the property snaps to its final value immediately.
  assert!(set_class(&mut doc, "box", "b no"));
  let snapped = doc.render_frame()?;
  assert_eq!(pixel(&snapped, 50, 50), (0, 0, 0, 255));

  // Verify the element remains at the final value after cancellation.
  doc.set_animation_time_ms(600.0);
  let after = doc.render_frame()?;
  assert_eq!(pixel(&after, 50, 50), (0, 0, 0, 255));

  doc.set_animation_time_ms(1000.0);
  let end = doc.render_frame()?;
  assert_eq!(pixel(&end, 50, 50), (0, 0, 0, 255));

  Ok(())
}

#[test]
fn setting_duration_to_zero_does_not_cancel_running_transition() -> Result<()> {
  ensure_test_env();

  let html = r#"
    <style>
      html, body { margin: 0; background: rgb(255, 255, 255); }
      #box {
        width: 100px;
        height: 100px;
        background: black;
        opacity: 0;
        transition-property: opacity;
        transition-timing-function: linear;
        transition-delay: 0ms;
        transition-duration: 1000ms;
      }
      #box.b { opacity: 1; transition-duration: 1000ms; }
      #box.no { opacity: 1; transition-duration: 0ms; }
    </style>
    <div id="box"></div>
  "#;

  let mut doc = BrowserDocument::from_html(
    html,
    RenderOptions::new()
      .with_viewport(100, 100)
      .with_animation_time(0.0),
  )?;

  doc.render_frame()?;

  // Start the transition at t=0.
  assert!(set_class(&mut doc, "box", "b"));
  doc.render_frame()?;

  // Sample mid-transition at t=500ms.
  doc.set_animation_time_ms(500.0);
  let mid = doc.render_frame()?;
  assert_mid_grey(pixel(&mid, 50, 50));

  // Set the duration to 0ms at t=500ms. Transition parameters are frozen, so the already running
  // transition continues with the 1000ms duration captured at start.
  assert!(set_class(&mut doc, "box", "b no"));
  let still_mid = doc.render_frame()?;
  assert_mid_grey(pixel(&still_mid, 50, 50));

  doc.set_animation_time_ms(600.0);
  let after = doc.render_frame()?;
  assert_grey_range(pixel(&after, 50, 50), 95..=110);

  doc.set_animation_time_ms(1000.0);
  let end = doc.render_frame()?;
  assert_eq!(pixel(&end, 50, 50), (0, 0, 0, 255));

  Ok(())
}

#[test]
fn transition_parameters_from_after_change_style_control_started_transition() -> Result<()> {
  ensure_test_env();

  let html = r#"
    <style>
      #box { width: 100px; height: 100px; background: black; opacity: 0; transition: opacity 1000ms linear; }
      #box:hover { opacity: 1; transition-duration: 2000ms; }
    </style>
    <div id="box"></div>
  "#;

  let mut doc = BrowserDocument::from_html(
    html,
    RenderOptions::new()
      .with_viewport(200, 200)
      .with_animation_time(0.0),
  )?;

  doc.render_frame()?;
  assert!(set_attr(&mut doc, "box", "data-fastr-hover", "true"));
  // Keep time at t=0 so this frame records the transition start time.
  doc.render_frame()?;

  let prepared = doc.prepared().expect("prepared");
  let box_id = box_id_by_element_id(prepared, "box");
  let base_tree = prepared.fragment_tree().clone();

  let eps = 1e-3;
  let cases = [(1000.0, 0.5), (2000.0, 1.0)];
  for (time, expected) in cases {
    let mut sampled = base_tree.clone();
    let viewport = sampled.viewport_size();
    animation::apply_transitions(&mut sampled, time, viewport);
    let opacity = fragment_opacity(&sampled, box_id);
    assert!(
      (opacity - expected).abs() < eps,
      "t={time} expected {expected}, got {opacity}"
    );
  }

  // Start the reverse transition at t=2000ms.
  doc.set_animation_time_ms(2000.0);
  assert!(remove_attr(&mut doc, "box", "data-fastr-hover"));
  doc.render_frame()?;

  let prepared = doc.prepared().expect("prepared");
  let box_id = box_id_by_element_id(prepared, "box");
  let base_tree = prepared.fragment_tree().clone();

  let cases = [(2500.0, 0.5), (3000.0, 0.0)];
  for (time, expected) in cases {
    let mut sampled = base_tree.clone();
    let viewport = sampled.viewport_size();
    animation::apply_transitions(&mut sampled, time, viewport);
    let opacity = fragment_opacity(&sampled, box_id);
    assert!(
      (opacity - expected).abs() < eps,
      "t={time} expected {expected}, got {opacity}"
    );
  }

  Ok(())
}

#[test]
fn transition_parameters_are_frozen_while_running() -> Result<()> {
  ensure_test_env();

  let html = r#"
    <style>
      #box {
        width: 100px;
        height: 100px;
        background: black;
        opacity: 0;
        transition-property: opacity;
        transition-timing-function: linear;
        transition-delay: 0s;
      }
      #box.state1 { opacity: 1; transition-duration: 2000ms; }
      #box.state2 { opacity: 1; transition-duration: 100ms; }
    </style>
    <div id="box"></div>
  "#;

  let mut doc = BrowserDocument::from_html(
    html,
    RenderOptions::new()
      .with_viewport(200, 200)
      .with_animation_time(0.0),
  )?;

  doc.render_frame()?;
  assert!(set_class(&mut doc, "box", "state1"));
  // Keep time at t=0 so this frame records the transition start time.
  doc.render_frame()?;

  doc.set_animation_time_ms(500.0);
  assert!(set_class(&mut doc, "box", "state2"));
  doc.render_frame()?;

  let prepared = doc.prepared().expect("prepared");
  let box_id = box_id_by_element_id(prepared, "box");
  let base_tree = prepared.fragment_tree().clone();

  let mut sampled = base_tree.clone();
  let viewport = sampled.viewport_size();
  animation::apply_transitions(&mut sampled, 1000.0, viewport);
  let opacity = fragment_opacity(&sampled, box_id);
  assert!(
    (opacity - 0.5).abs() < 1e-3,
    "expected opacity ~0.5 at 1000ms of 2000ms transition, got {opacity}"
  );

  Ok(())
}

#[test]
fn class_flip_triggers_transition_registered_custom_property_opacity_var() -> Result<()> {
  ensure_test_env();

  let html = r#"
    <style>
      @property --x { syntax: "<number>"; inherits: false; initial-value: 0; }
      #box { width: 100px; height: 100px; opacity: var(--x); transition: --x 1000ms linear; }
      .a { --x: 0; }
      .b { --x: 1; }
    </style>
    <div id="box" class="a"></div>
  "#;

  let mut doc = BrowserDocument::from_html(
    html,
    RenderOptions::new()
      .with_viewport(200, 200)
      .with_animation_time(0.0),
  )?;
  doc.render_frame()?;

  assert!(set_class(&mut doc, "box", "b"));
  // Keep time at t=0 so this frame records the transition start time.
  doc.render_frame()?;

  let prepared = doc.prepared().expect("prepared");
  let box_id = box_id_by_element_id(prepared, "box");
  let base_tree = prepared.fragment_tree().clone();

  let cases = [(0.0, 0.0), (500.0, 0.5), (1000.0, 1.0)];
  for (time, expected) in cases {
    let mut sampled = base_tree.clone();
    let viewport = sampled.viewport_size();
    animation::apply_transitions(&mut sampled, time, viewport);
    let opacity = fragment_opacity(&sampled, box_id);
    assert!(
      (opacity - expected).abs() < 1e-3,
      "t={time} expected {expected}, got {opacity}"
    );
  }

  Ok(())
}

#[test]
fn transition_all_includes_registered_custom_properties_on_class_change() -> Result<()> {
  ensure_test_env();

  let html = r#"
    <style>
      @property --x { syntax: "<number>"; inherits: false; initial-value: 0; }
      #box { width: 100px; height: 100px; opacity: var(--x); transition: all 1000ms linear; }
      .a { --x: 0; }
      .b { --x: 1; }
    </style>
    <div id="box" class="a"></div>
  "#;

  let mut doc = BrowserDocument::from_html(
    html,
    RenderOptions::new()
      .with_viewport(200, 200)
      .with_animation_time(0.0),
  )?;
  doc.render_frame()?;

  assert!(set_class(&mut doc, "box", "b"));
  doc.render_frame()?;

  let prepared = doc.prepared().expect("prepared");
  let box_id = box_id_by_element_id(prepared, "box");
  let base_tree = prepared.fragment_tree().clone();

  let mut sampled = base_tree.clone();
  let viewport = sampled.viewport_size();
  animation::apply_transitions(&mut sampled, 500.0, viewport);

  let frag = find_fragment(&sampled.root, box_id).expect("fragment present");
  let style = frag.style.as_ref().expect("style present");
  let value = style
    .custom_properties
    .get("--x")
    .expect("custom property present");
  match value.typed.as_ref() {
    Some(CustomPropertyTypedValue::Number(v)) => assert!((v - 0.5).abs() < 1e-3, "v={v}"),
    other => panic!("expected typed number, got {other:?}"),
  }
  assert!((style.opacity - 0.5).abs() < 1e-3, "opacity={}", style.opacity);

  Ok(())
}

#[test]
fn inherited_color_tracks_parent_transition_even_through_line_fragments() -> Result<()> {
  ensure_test_env();

  let html = r#"
    <style>
      #parent { color: rgb(0, 0, 0); transition: color 1000ms linear; }
      #parent.b { color: rgb(255, 255, 255); }
      /* Ensure the child does not start its own transition; it should inherit the parent's animated color. */
      #child { transition: none; }
    </style>
    <div id="parent"><span id="child">X</span></div>
  "#;

  let mut doc = BrowserDocument::from_html(
    html,
    RenderOptions::new()
      .with_viewport(200, 50)
      .with_animation_time(0.0),
  )?;
  doc.render_frame()?;

  assert!(set_class(&mut doc, "parent", "b"));
  // Keep time at t=0 so this frame records the transition start time.
  doc.render_frame()?;

  let prepared = doc.prepared().expect("prepared");
  let parent_id = box_id_by_element_id(prepared, "parent");
  let child_id = box_id_by_element_id(prepared, "child");
  let mut sampled = prepared.fragment_tree().clone();
  let viewport = sampled.viewport_size();

  animation::apply_transitions(&mut sampled, 500.0, viewport);

  let expected = Rgba::new(128, 128, 128, 1.0);
  assert_eq!(fragment_color(&sampled, parent_id), expected, "parent color");
  assert_eq!(
    fragment_color(&sampled, child_id),
    expected,
    "child should inherit parent's animated color"
  );

  Ok(())
}

#[test]
fn currentcolor_dependent_border_color_tracks_color_transition() -> Result<()> {
  ensure_test_env();

  let html = r#"
    <style>
      #box {
        width: 20px;
        height: 20px;
        color: rgb(0, 0, 0);
        border-top-style: solid;
        border-top-width: 1px;
        border-top-color: currentColor;
        transition: color 1000ms linear;
      }
      #box.b { color: rgb(255, 255, 255); }
    </style>
    <div id="box"></div>
  "#;

  let mut doc = BrowserDocument::from_html(
    html,
    RenderOptions::new()
      .with_viewport(200, 50)
      .with_animation_time(0.0),
  )?;
  doc.render_frame()?;

  assert!(set_class(&mut doc, "box", "b"));
  // Keep time at t=0 so this frame records the transition start time.
  doc.render_frame()?;

  let prepared = doc.prepared().expect("prepared");
  let box_id = box_id_by_element_id(prepared, "box");
  let mut sampled = prepared.fragment_tree().clone();
  let viewport = sampled.viewport_size();

  animation::apply_transitions(&mut sampled, 500.0, viewport);

  let expected = Rgba::new(128, 128, 128, 1.0);
  assert_eq!(fragment_color(&sampled, box_id), expected, "animated text color");
  assert_eq!(
    fragment_border_top_color(&sampled, box_id),
    expected,
    "border-top-color: currentColor should track animated color"
  );

  Ok(())
}
