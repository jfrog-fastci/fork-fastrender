use fastrender::api::{FastRender, RenderOptions};
use fastrender::style::cascade::StyledNode;
use fastrender::style::color::Rgba;
use fastrender::tree::box_tree::BoxNode;

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
