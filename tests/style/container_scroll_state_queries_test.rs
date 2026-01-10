use fastrender::api::{FastRender, RenderOptions};
use fastrender::scroll::{ScrollBounds, ScrollChainState};
use fastrender::style::cascade::StyledNode;
use fastrender::style::color::Rgba;
use fastrender::tree::fragment_tree::{FragmentNode, FragmentTree};
use fastrender::tree::box_tree::BoxNode;
use fastrender::{Point, Size};

fn find_by_id<'a>(node: &'a StyledNode, id: &str) -> Option<&'a StyledNode> {
  if node.node.get_attribute_ref("id") == Some(id) {
    return Some(node);
  }
  node.children.iter().find_map(|child| find_by_id(child, id))
}

fn box_id_by_element_id(node: &BoxNode, target_id: &str) -> Option<usize> {
  if let Some(debug) = node.debug_info.as_ref() {
    if debug.id.as_deref() == Some(target_id) {
      return Some(node.id);
    }
  }
  node
    .children
    .iter()
    .find_map(|child| box_id_by_element_id(child, target_id))
}

fn scroll_bounds_for_box_id(tree: &FragmentTree, box_id: usize) -> Option<ScrollBounds> {
  fn merge_bounds(existing: &mut ScrollBounds, next: ScrollBounds) {
    existing.min_x = existing.min_x.min(next.min_x);
    existing.min_y = existing.min_y.min(next.min_y);
    existing.max_x = existing.max_x.max(next.max_x);
    existing.max_y = existing.max_y.max(next.max_y);
  }

  fn walk(
    fragment: &FragmentNode,
    box_id: usize,
    viewport_for_units: Size,
    out: &mut Option<ScrollBounds>,
  ) {
    if fragment.box_id() == Some(box_id) {
      if let Some(state) = ScrollChainState::from_fragment(
        fragment,
        Point::ZERO,
        viewport_for_units,
        viewport_for_units,
        false,
        false,
      ) {
        match out.as_mut() {
          Some(existing) => merge_bounds(existing, state.bounds),
          None => *out = Some(state.bounds),
        }
      }
    }

    for child in fragment.children.iter() {
      walk(child, box_id, viewport_for_units, out);
    }
  }

  let viewport_for_units = tree.viewport_size();
  let mut out = None;
  walk(&tree.root, box_id, viewport_for_units, &mut out);
  for fragment in tree.additional_fragments.iter() {
    walk(fragment, box_id, viewport_for_units, &mut out);
  }
  out
}

#[test]
fn container_scroll_state_scrollable_bottom_tracks_element_scroll_offset() {
  let html = r#"
    <style>
      #scroller {
        width: 80px;
        height: 60px;
        overflow-y: auto;
        overflow-x: hidden;
        container-name: scroller;
      }
      #spacer { height: 200px; }
      #target { color: rgb(0, 0, 255); }
      @container scroller scroll-state(scrollable: bottom) {
        #target { color: rgb(255, 0, 0); }
      }
    </style>
    <div id="scroller">
      <div id="spacer"></div>
      <div id="target">hello</div>
    </div>
  "#;

  let mut renderer = FastRender::new().expect("renderer");
  let base_options = RenderOptions::new().with_viewport(80, 60);

  let prepared_top = renderer
    .prepare_html(html, base_options.clone())
    .expect("prepare top");
  let scroller_box_id =
    box_id_by_element_id(&prepared_top.box_tree().root, "scroller").expect("scroller box id");

  let target_top = find_by_id(prepared_top.styled_tree(), "target").expect("target element");
  assert_eq!(
    target_top.styles.color,
    Rgba::rgb(255, 0, 0),
    "expected scrollable: bottom to match at scroll start"
  );

  let prepared_bottom = renderer
    .prepare_html(
      html,
      base_options
        .clone()
        .with_element_scroll(scroller_box_id, 0.0, 10_000.0),
    )
    .expect("prepare bottom");

  let target_bottom = find_by_id(prepared_bottom.styled_tree(), "target").expect("target element");
  assert_eq!(
    target_bottom.styles.color,
    Rgba::rgb(0, 0, 255),
    "expected scrollable: bottom to be false at scroll end"
  );
}

#[test]
fn container_scroll_state_scrollable_top_tracks_element_scroll_offset() {
  let html = r#"
    <style>
      #scroller {
        width: 80px;
        height: 60px;
        overflow-y: auto;
        overflow-x: hidden;
        container-name: scroller;
      }
      #spacer { height: 200px; }
      #target { color: rgb(0, 0, 255); }
      @container scroller scroll-state(scrollable: top) {
        #target { color: rgb(255, 0, 0); }
      }
    </style>
    <div id="scroller">
      <div id="spacer"></div>
      <div id="target">hello</div>
    </div>
  "#;

  let mut renderer = FastRender::new().expect("renderer");
  let base_options = RenderOptions::new().with_viewport(80, 60);

  let prepared_top = renderer
    .prepare_html(html, base_options.clone())
    .expect("prepare top");
  let scroller_box_id =
    box_id_by_element_id(&prepared_top.box_tree().root, "scroller").expect("scroller box id");

  let target_top = find_by_id(prepared_top.styled_tree(), "target").expect("target element");
  assert_eq!(
    target_top.styles.color,
    Rgba::rgb(0, 0, 255),
    "expected scrollable: top to be false at scroll start"
  );

  let prepared_bottom = renderer
    .prepare_html(
      html,
      base_options
        .clone()
        .with_element_scroll(scroller_box_id, 0.0, 10_000.0),
    )
    .expect("prepare bottom");

  let target_bottom = find_by_id(prepared_bottom.styled_tree(), "target").expect("target element");
  assert_eq!(
    target_bottom.styles.color,
    Rgba::rgb(255, 0, 0),
    "expected scrollable: top to match at scroll end"
  );
}

#[test]
fn container_scroll_state_scrollable_right_tracks_element_scroll_offset() {
  let html = r#"
    <style>
      #scroller {
        width: 80px;
        height: 60px;
        overflow-x: auto;
        overflow-y: hidden;
        container-name: scroller;
      }
      #spacer { width: 200px; height: 1px; }
      #target { color: rgb(0, 0, 255); }
      @container scroller scroll-state(scrollable: right) {
        #target { color: rgb(255, 0, 0); }
      }
    </style>
    <div id="scroller">
      <div id="spacer"></div>
      <div id="target">hello</div>
    </div>
  "#;

  let mut renderer = FastRender::new().expect("renderer");
  let base_options = RenderOptions::new().with_viewport(80, 60);

  let prepared_left = renderer
    .prepare_html(html, base_options.clone())
    .expect("prepare left");
  let scroller_box_id =
    box_id_by_element_id(&prepared_left.box_tree().root, "scroller").expect("scroller box id");

  let target_left = find_by_id(prepared_left.styled_tree(), "target").expect("target element");
  assert_eq!(
    target_left.styles.color,
    Rgba::rgb(255, 0, 0),
    "expected scrollable: right to match at scroll start"
  );

  let prepared_right = renderer
    .prepare_html(
      html,
      base_options
        .clone()
        .with_element_scroll(scroller_box_id, 10_000.0, 0.0),
    )
    .expect("prepare right");

  let target_right = find_by_id(prepared_right.styled_tree(), "target").expect("target element");
  assert_eq!(
    target_right.styles.color,
    Rgba::rgb(0, 0, 255),
    "expected scrollable: right to be false at scroll end"
  );
}

#[test]
fn container_scroll_state_scrollable_left_tracks_element_scroll_offset() {
  let html = r#"
    <style>
      #scroller {
        width: 80px;
        height: 60px;
        overflow-x: auto;
        overflow-y: hidden;
        container-name: scroller;
      }
      #spacer { width: 200px; height: 1px; }
      #target { color: rgb(0, 0, 255); }
      @container scroller scroll-state(scrollable: left) {
        #target { color: rgb(255, 0, 0); }
      }
    </style>
    <div id="scroller">
      <div id="spacer"></div>
      <div id="target">hello</div>
    </div>
  "#;

  let mut renderer = FastRender::new().expect("renderer");
  let base_options = RenderOptions::new().with_viewport(80, 60);

  let prepared_left = renderer
    .prepare_html(html, base_options.clone())
    .expect("prepare left");
  let scroller_box_id =
    box_id_by_element_id(&prepared_left.box_tree().root, "scroller").expect("scroller box id");

  let target_left = find_by_id(prepared_left.styled_tree(), "target").expect("target element");
  assert_eq!(
    target_left.styles.color,
    Rgba::rgb(0, 0, 255),
    "expected scrollable: left to be false at scroll start"
  );

  let prepared_right = renderer
    .prepare_html(
      html,
      base_options
        .clone()
        .with_element_scroll(scroller_box_id, 10_000.0, 0.0),
    )
    .expect("prepare right");

  let target_right = find_by_id(prepared_right.styled_tree(), "target").expect("target element");
  assert_eq!(
    target_right.styles.color,
    Rgba::rgb(255, 0, 0),
    "expected scrollable: left to match at scroll end"
  );
}

#[test]
fn container_scroll_state_scrollable_x_matches_horizontal_scroll_containers() {
  let html = r#"
    <style>
      #scroller {
        width: 80px;
        height: 60px;
        overflow-x: auto;
        overflow-y: hidden;
        container-name: scroller;
      }
      #spacer { width: 200px; height: 1px; }
      #target { color: rgb(0, 0, 255); }
      @container scroller scroll-state(scrollable: x) {
        #target { color: rgb(255, 0, 0); }
      }
    </style>
    <div id="scroller">
      <div id="spacer"></div>
      <div id="target">hello</div>
    </div>
  "#;

  let mut renderer = FastRender::new().expect("renderer");
  let prepared = renderer
    .prepare_html(html, RenderOptions::new().with_viewport(80, 60))
    .expect("prepare");

  let target = find_by_id(prepared.styled_tree(), "target").expect("target element");
  assert_eq!(
    target.styles.color,
    Rgba::rgb(255, 0, 0),
    "expected scrollable: x to match for horizontal scrollers"
  );
}

#[test]
fn container_scroll_state_scrollable_none_matches_non_scrollable_containers() {
  let html = r#"
    <style>
      #scroller {
        width: 80px;
        height: 60px;
        overflow-y: auto;
        overflow-x: hidden;
        container-name: scroller;
      }
      #target { color: rgb(0, 0, 255); }
      @container scroller scroll-state(scrollable: none) {
        #target { color: rgb(255, 0, 0); }
      }
    </style>
    <div id="scroller">
      <div id="target">hello</div>
    </div>
  "#;

  let mut renderer = FastRender::new().expect("renderer");
  let prepared = renderer
    .prepare_html(html, RenderOptions::new().with_viewport(80, 60))
    .expect("prepare");

  let target = find_by_id(prepared.styled_tree(), "target").expect("target element");
  assert_eq!(
    target.styles.color,
    Rgba::rgb(255, 0, 0),
    "expected scrollable: none to match when there is no overflow"
  );
}

#[test]
fn container_scroll_state_scrollable_inline_start_respects_rtl_direction() {
  let html = r#"
    <style>
      #scroller {
        width: 80px;
        height: 60px;
        overflow-x: auto;
        overflow-y: hidden;
        direction: rtl;
        container-name: scroller;
      }
      #spacer { width: 200px; height: 1px; }
      #target { color: rgb(0, 0, 255); }
      @container scroller scroll-state(scrollable: inline-start) {
        #target { color: rgb(255, 0, 0); }
      }
    </style>
    <div id="scroller">
      <div id="spacer"></div>
      <div id="target">hello</div>
    </div>
  "#;

  let mut renderer = FastRender::new().expect("renderer");
  let base_options = RenderOptions::new().with_viewport(80, 60);

  let prepared = renderer
    .prepare_html(html, base_options.clone())
    .expect("prepare");
  let scroller_box_id =
    box_id_by_element_id(&prepared.box_tree().root, "scroller").expect("scroller box id");
  let bounds =
    scroll_bounds_for_box_id(prepared.fragment_tree(), scroller_box_id).expect("scroll bounds");

  let prepared_min = renderer
    .prepare_html(
      html,
      base_options
        .clone()
        .with_element_scroll(scroller_box_id, bounds.min_x, 0.0),
    )
    .expect("prepare min");
  let target_min = find_by_id(prepared_min.styled_tree(), "target").expect("target element");
  assert_eq!(
    target_min.styles.color,
    Rgba::rgb(255, 0, 0),
    "expected scrollable: inline-start to match when not at the inline-start edge"
  );

  let prepared_max = renderer
    .prepare_html(
      html,
      base_options
        .clone()
        .with_element_scroll(scroller_box_id, bounds.max_x, 0.0),
    )
    .expect("prepare max");
  let target_max = find_by_id(prepared_max.styled_tree(), "target").expect("target element");
  assert_eq!(
    target_max.styles.color,
    Rgba::rgb(0, 0, 255),
    "expected scrollable: inline-start to be false at the inline-start edge"
  );
}

#[test]
fn container_scroll_state_scrollable_inline_end_respects_rtl_direction() {
  let html = r#"
    <style>
      #scroller {
        width: 80px;
        height: 60px;
        overflow-x: auto;
        overflow-y: hidden;
        direction: rtl;
        container-name: scroller;
      }
      #spacer { width: 200px; height: 1px; }
      #target { color: rgb(0, 0, 255); }
      @container scroller scroll-state(scrollable: inline-end) {
        #target { color: rgb(255, 0, 0); }
      }
    </style>
    <div id="scroller">
      <div id="spacer"></div>
      <div id="target">hello</div>
    </div>
  "#;

  let mut renderer = FastRender::new().expect("renderer");
  let base_options = RenderOptions::new().with_viewport(80, 60);

  let prepared = renderer
    .prepare_html(html, base_options.clone())
    .expect("prepare");
  let scroller_box_id =
    box_id_by_element_id(&prepared.box_tree().root, "scroller").expect("scroller box id");
  let bounds =
    scroll_bounds_for_box_id(prepared.fragment_tree(), scroller_box_id).expect("scroll bounds");

  let prepared_min = renderer
    .prepare_html(
      html,
      base_options
        .clone()
        .with_element_scroll(scroller_box_id, bounds.min_x, 0.0),
    )
    .expect("prepare min");
  let target_min = find_by_id(prepared_min.styled_tree(), "target").expect("target element");
  assert_eq!(
    target_min.styles.color,
    Rgba::rgb(0, 0, 255),
    "expected scrollable: inline-end to be false at the inline-end edge"
  );

  let prepared_max = renderer
    .prepare_html(
      html,
      base_options
        .clone()
        .with_element_scroll(scroller_box_id, bounds.max_x, 0.0),
    )
    .expect("prepare max");
  let target_max = find_by_id(prepared_max.styled_tree(), "target").expect("target element");
  assert_eq!(
    target_max.styles.color,
    Rgba::rgb(255, 0, 0),
    "expected scrollable: inline-end to match when not at the inline-end edge"
  );
}

#[test]
fn container_scroll_state_scrollable_boolean_matches_even_at_scroll_end() {
  let html = r#"
    <style>
      #scroller {
        width: 80px;
        height: 60px;
        overflow-y: auto;
        overflow-x: hidden;
        container-name: scroller;
      }
      #spacer { height: 200px; }
      #target { color: rgb(0, 0, 255); }
      @container scroller scroll-state(scrollable) {
        #target { color: rgb(255, 0, 0); }
      }
    </style>
    <div id="scroller">
      <div id="spacer"></div>
      <div id="target">hello</div>
    </div>
  "#;

  let mut renderer = FastRender::new().expect("renderer");
  let base_options = RenderOptions::new().with_viewport(80, 60);

  let prepared_top = renderer
    .prepare_html(html, base_options.clone())
    .expect("prepare top");
  let scroller_box_id =
    box_id_by_element_id(&prepared_top.box_tree().root, "scroller").expect("scroller box id");

  let target_top = find_by_id(prepared_top.styled_tree(), "target").expect("target element");
  assert_eq!(
    target_top.styles.color,
    Rgba::rgb(255, 0, 0),
    "expected scrollable to match for scroll containers at scroll start"
  );

  let prepared_bottom = renderer
    .prepare_html(
      html,
      base_options
        .clone()
        .with_element_scroll(scroller_box_id, 0.0, 10_000.0),
    )
    .expect("prepare bottom");
  let target_bottom = find_by_id(prepared_bottom.styled_tree(), "target").expect("target element");
  assert_eq!(
    target_bottom.styles.color,
    Rgba::rgb(255, 0, 0),
    "expected scrollable to match for scroll containers even at scroll end"
  );
}

#[test]
fn container_scroll_state_scrollable_block_end_tracks_element_scroll_offset() {
  let html = r#"
    <style>
      #scroller {
        width: 80px;
        height: 60px;
        overflow-y: auto;
        overflow-x: hidden;
        container-name: scroller;
      }
      #spacer { height: 200px; }
      #target { color: rgb(0, 0, 255); }
      @container scroller scroll-state(scrollable: block-end) {
        #target { color: rgb(255, 0, 0); }
      }
    </style>
    <div id="scroller">
      <div id="spacer"></div>
      <div id="target">hello</div>
    </div>
  "#;

  let mut renderer = FastRender::new().expect("renderer");
  let base_options = RenderOptions::new().with_viewport(80, 60);

  let prepared_top = renderer
    .prepare_html(html, base_options.clone())
    .expect("prepare top");
  let scroller_box_id =
    box_id_by_element_id(&prepared_top.box_tree().root, "scroller").expect("scroller box id");

  let target_top = find_by_id(prepared_top.styled_tree(), "target").expect("target element");
  assert_eq!(
    target_top.styles.color,
    Rgba::rgb(255, 0, 0),
    "expected scrollable: block-end to match at scroll start"
  );

  let prepared_bottom = renderer
    .prepare_html(
      html,
      base_options
        .clone()
        .with_element_scroll(scroller_box_id, 0.0, 10_000.0),
    )
    .expect("prepare bottom");
  let target_bottom = find_by_id(prepared_bottom.styled_tree(), "target").expect("target element");
  assert_eq!(
    target_bottom.styles.color,
    Rgba::rgb(0, 0, 255),
    "expected scrollable: block-end to be false at scroll end"
  );
}

#[test]
fn container_scroll_state_scrollable_inline_keyword_matches_horizontal_scroll_containers() {
  let html = r#"
    <style>
      #scroller {
        width: 80px;
        height: 60px;
        overflow-x: auto;
        overflow-y: hidden;
        container-name: scroller;
      }
      #spacer { width: 200px; height: 1px; }
      #target { color: rgb(0, 0, 255); }
      @container scroller scroll-state(scrollable: inline) {
        #target { color: rgb(255, 0, 0); }
      }
    </style>
    <div id="scroller">
      <div id="spacer"></div>
      <div id="target">hello</div>
    </div>
  "#;

  let mut renderer = FastRender::new().expect("renderer");
  let prepared = renderer
    .prepare_html(html, RenderOptions::new().with_viewport(80, 60))
    .expect("prepare");

  let target = find_by_id(prepared.styled_tree(), "target").expect("target element");
  assert_eq!(
    target.styles.color,
    Rgba::rgb(255, 0, 0),
    "expected scrollable: inline to match for horizontal scrollers in horizontal writing mode"
  );
}

#[test]
fn container_scroll_state_scrollable_block_keyword_matches_vertical_scroll_containers() {
  let html = r#"
    <style>
      #scroller {
        width: 80px;
        height: 60px;
        overflow-y: auto;
        overflow-x: hidden;
        container-name: scroller;
      }
      #spacer { height: 200px; }
      #target { color: rgb(0, 0, 255); }
      @container scroller scroll-state(scrollable: block) {
        #target { color: rgb(255, 0, 0); }
      }
    </style>
    <div id="scroller">
      <div id="spacer"></div>
      <div id="target">hello</div>
    </div>
  "#;

  let mut renderer = FastRender::new().expect("renderer");
  let prepared = renderer
    .prepare_html(html, RenderOptions::new().with_viewport(80, 60))
    .expect("prepare");

  let target = find_by_id(prepared.styled_tree(), "target").expect("target element");
  assert_eq!(
    target.styles.color,
    Rgba::rgb(255, 0, 0),
    "expected scrollable: block to match for vertical scrollers in horizontal writing mode"
  );
}

#[test]
fn container_scroll_state_scrollable_top_tracks_viewport_scroll_offset() {
  let html = r#"
    <style>
      html {
        container-name: viewport;
      }
      html, body { margin: 0; }
      #spacer { height: 2000px; }
      #target { color: rgb(0, 0, 255); }
      @container viewport scroll-state(scrollable: top) {
        #target { color: rgb(255, 0, 0); }
      }
    </style>
    <div id="spacer"></div>
    <div id="target">hello</div>
  "#;

  let mut renderer = FastRender::new().expect("renderer");
  let base_options = RenderOptions::new().with_viewport(80, 60);

  let prepared_top = renderer
    .prepare_html(html, base_options.clone())
    .expect("prepare top");
  let target_top = find_by_id(prepared_top.styled_tree(), "target").expect("target element");
  assert_eq!(
    target_top.styles.color,
    Rgba::rgb(0, 0, 255),
    "expected scrollable: top to be false at scroll start for viewport scrolling"
  );

  let prepared_bottom = renderer
    .prepare_html(html, base_options.clone().with_scroll(0.0, 10_000.0))
    .expect("prepare bottom");
  let target_bottom = find_by_id(prepared_bottom.styled_tree(), "target").expect("target element");
  assert_eq!(
    target_bottom.styles.color,
    Rgba::rgb(255, 0, 0),
    "expected scrollable: top to match at scroll end for viewport scrolling"
  );
}

#[test]
fn container_scroll_state_stuck_top_tracks_viewport_scroll_offset() {
  let html = r#"
    <style>
      html, body { margin: 0; }
      #spacer { height: 200px; }
      #after { height: 2000px; }
      #sticky {
        position: sticky;
        top: 0;
        container-name: sticky;
      }
      #target { color: rgb(0, 0, 255); }
      @container sticky scroll-state(stuck: top) {
        #target { color: rgb(255, 0, 0); }
      }
    </style>
    <div id="spacer"></div>
    <div id="sticky"><div id="target">hello</div></div>
    <div id="after"></div>
  "#;

  let mut renderer = FastRender::new().expect("renderer");
  let base_options = RenderOptions::new().with_viewport(80, 60);

  let prepared_top = renderer
    .prepare_html(html, base_options.clone())
    .expect("prepare top");
  let target_top = find_by_id(prepared_top.styled_tree(), "target").expect("target element");
  assert_eq!(
    target_top.styles.color,
    Rgba::rgb(0, 0, 255),
    "expected stuck: top to be false before scrolling"
  );

  let prepared_stuck = renderer
    .prepare_html(html, base_options.clone().with_scroll(0.0, 500.0))
    .expect("prepare stuck");
  let target_stuck = find_by_id(prepared_stuck.styled_tree(), "target").expect("target element");
  assert_eq!(
    target_stuck.styles.color,
    Rgba::rgb(255, 0, 0),
    "expected stuck: top to match after scrolling past the sticky element"
  );
}

#[test]
fn container_scroll_state_scrollable_inline_start_in_vertical_writing_mode_respects_rtl_direction() {
  let html = r#"
    <style>
      #scroller {
        width: 80px;
        height: 60px;
        overflow-y: auto;
        overflow-x: hidden;
        writing-mode: vertical-rl;
        direction: rtl;
        container-name: scroller;
      }
      #spacer { height: 200px; }
      #target { color: rgb(0, 0, 255); }
      @container scroller scroll-state(scrollable: inline-start) {
        #target { color: rgb(255, 0, 0); }
      }
    </style>
    <div id="scroller">
      <div id="spacer"></div>
      <div id="target">hello</div>
    </div>
  "#;

  let mut renderer = FastRender::new().expect("renderer");
  let base_options = RenderOptions::new().with_viewport(80, 60);

  let prepared = renderer
    .prepare_html(html, base_options.clone())
    .expect("prepare");
  let scroller_box_id =
    box_id_by_element_id(&prepared.box_tree().root, "scroller").expect("scroller box id");
  let bounds = scroll_bounds_for_box_id(prepared.fragment_tree(), scroller_box_id).expect("bounds");

  let prepared_min = renderer
    .prepare_html(
      html,
      base_options
        .clone()
        .with_element_scroll(scroller_box_id, 0.0, bounds.min_y),
    )
    .expect("prepare min");
  let target_min = find_by_id(prepared_min.styled_tree(), "target").expect("target element");
  assert_eq!(
    target_min.styles.color,
    Rgba::rgb(255, 0, 0),
    "expected scrollable: inline-start to match when not at the inline-start edge in vertical writing mode"
  );

  let prepared_max = renderer
    .prepare_html(
      html,
      base_options
        .clone()
        .with_element_scroll(scroller_box_id, 0.0, bounds.max_y),
    )
    .expect("prepare max");
  let target_max = find_by_id(prepared_max.styled_tree(), "target").expect("target element");
  assert_eq!(
    target_max.styles.color,
    Rgba::rgb(0, 0, 255),
    "expected scrollable: inline-start to be false at the inline-start edge in vertical writing mode"
  );
}

#[test]
fn container_scroll_state_scrollable_block_end_in_vertical_writing_mode_tracks_horizontal_scroll_offset() {
  let html = r#"
    <style>
      #scroller {
        width: 80px;
        height: 60px;
        overflow-x: auto;
        overflow-y: hidden;
        writing-mode: vertical-rl;
        container-name: scroller;
      }
      #spacer { width: 200px; height: 1px; }
      #target { color: rgb(0, 0, 255); }
      @container scroller scroll-state(scrollable: block-end) {
        #target { color: rgb(255, 0, 0); }
      }
    </style>
    <div id="scroller">
      <div id="spacer"></div>
      <div id="target">hello</div>
    </div>
  "#;

  let mut renderer = FastRender::new().expect("renderer");
  let base_options = RenderOptions::new().with_viewport(80, 60);

  let prepared = renderer
    .prepare_html(html, base_options.clone())
    .expect("prepare");
  let scroller_box_id =
    box_id_by_element_id(&prepared.box_tree().root, "scroller").expect("scroller box id");
  let bounds = scroll_bounds_for_box_id(prepared.fragment_tree(), scroller_box_id).expect("bounds");

  let prepared_min = renderer
    .prepare_html(
      html,
      base_options
        .clone()
        .with_element_scroll(scroller_box_id, bounds.min_x, 0.0),
    )
    .expect("prepare min");
  let target_min = find_by_id(prepared_min.styled_tree(), "target").expect("target element");
  assert_eq!(
    target_min.styles.color,
    Rgba::rgb(0, 0, 255),
    "expected scrollable: block-end to be false at the block-end edge in vertical writing mode"
  );

  let prepared_max = renderer
    .prepare_html(
      html,
      base_options
        .clone()
        .with_element_scroll(scroller_box_id, bounds.max_x, 0.0),
    )
    .expect("prepare max");
  let target_max = find_by_id(prepared_max.styled_tree(), "target").expect("target element");
  assert_eq!(
    target_max.styles.color,
    Rgba::rgb(255, 0, 0),
    "expected scrollable: block-end to match when not at the block-end edge in vertical writing mode"
  );
}

#[test]
fn container_scroll_state_scrollable_inline_keyword_matches_vertical_scroll_containers_in_vertical_writing_mode() {
  let html = r#"
    <style>
      #scroller {
        width: 80px;
        height: 60px;
        overflow-y: auto;
        overflow-x: hidden;
        writing-mode: vertical-rl;
        container-name: scroller;
      }
      #spacer { height: 200px; }
      #target { color: rgb(0, 0, 255); }
      @container scroller scroll-state(scrollable: inline) {
        #target { color: rgb(255, 0, 0); }
      }
    </style>
    <div id="scroller">
      <div id="spacer"></div>
      <div id="target">hello</div>
    </div>
  "#;

  let mut renderer = FastRender::new().expect("renderer");
  let prepared = renderer
    .prepare_html(html, RenderOptions::new().with_viewport(80, 60))
    .expect("prepare");

  let target = find_by_id(prepared.styled_tree(), "target").expect("target element");
  assert_eq!(
    target.styles.color,
    Rgba::rgb(255, 0, 0),
    "expected scrollable: inline to match for vertical scrollers in vertical writing mode"
  );
}

#[test]
fn container_scroll_state_scrollable_block_keyword_matches_horizontal_scroll_containers_in_vertical_writing_mode() {
  let html = r#"
    <style>
      #scroller {
        width: 80px;
        height: 60px;
        overflow-x: auto;
        overflow-y: hidden;
        writing-mode: vertical-rl;
        container-name: scroller;
      }
      #spacer { width: 200px; height: 1px; }
      #target { color: rgb(0, 0, 255); }
      @container scroller scroll-state(scrollable: block) {
        #target { color: rgb(255, 0, 0); }
      }
    </style>
    <div id="scroller">
      <div id="spacer"></div>
      <div id="target">hello</div>
    </div>
  "#;

  let mut renderer = FastRender::new().expect("renderer");
  let prepared = renderer
    .prepare_html(html, RenderOptions::new().with_viewport(80, 60))
    .expect("prepare");

  let target = find_by_id(prepared.styled_tree(), "target").expect("target element");
  assert_eq!(
    target.styles.color,
    Rgba::rgb(255, 0, 0),
    "expected scrollable: block to match for horizontal scrollers in vertical writing mode"
  );
}
