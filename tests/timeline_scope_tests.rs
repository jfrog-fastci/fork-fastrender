use std::collections::HashMap;

use fastrender::api::FastRender;
use fastrender::scroll::ScrollState;
use fastrender::style::types::TimelineScopeProperty;
use fastrender::Rgba;
use fastrender::{BoxNode, FragmentNode, FragmentTree, Point, PreparedPaintOptions, RenderOptions};

fn pixel(pixmap: &tiny_skia::Pixmap, x: u32, y: u32) -> (u8, u8, u8, u8) {
  let px = pixmap.pixel(x, y).unwrap();
  (px.red(), px.green(), px.blue(), px.alpha())
}

fn find_box_id_by_dom_id(node: &BoxNode, id: &str) -> Option<usize> {
  if node.debug_info.as_ref().and_then(|info| info.id.as_deref()) == Some(id) {
    return Some(node.id);
  }
  node
    .children
    .iter()
    .find_map(|child| find_box_id_by_dom_id(child, id))
}

fn find_fragment_by_box_id<'a>(tree: &'a FragmentTree, box_id: usize) -> Option<&'a FragmentNode> {
  fn rec<'a>(node: &'a FragmentNode, box_id: usize) -> Option<&'a FragmentNode> {
    if node.box_id() == Some(box_id) {
      return Some(node);
    }
    node.children.iter().find_map(|child| rec(child, box_id))
  }

  rec(&tree.root, box_id).or_else(|| {
    tree
      .additional_fragments
      .iter()
      .find_map(|frag| rec(frag, box_id))
  })
}

#[test]
fn timeline_scope_promotes_named_scroll_timeline_for_siblings() {
  let mut renderer = FastRender::new().expect("renderer");
  let options = RenderOptions::new().with_viewport(100, 100);

  let html_template = |with_scope: bool| {
    let scope_rule = if with_scope {
      "#container { timeline-scope: --scroller; }"
    } else {
      ""
    };
    format!(
      r#"
      <style>
        html, body {{ margin: 0; background: rgb(0, 0, 0); }}
        {scope_rule}
        #anim {{
          width: 100px;
          height: 50px;
          background: rgb(255, 0, 0);
          opacity: 0;
          animation-timeline: --scroller;
          animation: fade auto linear;
        }}
        #scroller {{
          overflow-y: scroll;
          height: 50px;
          width: 100px;
          scroll-timeline: --scroller block;
        }}
        @keyframes fade {{ from {{ opacity: 0; }} to {{ opacity: 1; }} }}
      </style>
      <div id="container">
        <div id="anim"></div>
        <div id="scroller"><div style="height: 150px;"></div></div>
      </div>
    "#
    )
  };

  let paint_at_bottom = |prepared: &fastrender::api::PreparedDocument| {
    let scroller_id =
      find_box_id_by_dom_id(&prepared.box_tree().root, "scroller").expect("scroller box_id");
    let scroller_frag =
      find_fragment_by_box_id(prepared.fragment_tree(), scroller_id).expect("scroller fragment");
    let max_scroll =
      (scroller_frag.scroll_overflow.height() - scroller_frag.bounds.height()).max(0.0);
    assert!(max_scroll > 0.0, "expected scroll range for scroller");

    let scroll_state = ScrollState::from_parts(
      Point::ZERO,
      HashMap::from([(scroller_id, Point::new(0.0, max_scroll))]),
    );
    prepared
      .paint_with_options(
        PreparedPaintOptions::new()
          .with_scroll_state(scroll_state)
          .with_background(Rgba::new(0, 0, 0, 1.0)),
      )
      .expect("paint")
  };

  let prepared_without_scope = renderer
    .prepare_html(&html_template(false), options.clone())
    .expect("prepare without scope");
  let container_id =
    find_box_id_by_dom_id(&prepared_without_scope.box_tree().root, "container").expect("container");
  let container_frag =
    find_fragment_by_box_id(prepared_without_scope.fragment_tree(), container_id)
      .expect("container fragment");
  let container_style = container_frag.style.as_deref().expect("container style");
  assert_eq!(container_style.timeline_scope, TimelineScopeProperty::None);
  let pixmap_without_scope = paint_at_bottom(&prepared_without_scope);
  // Without promotion, the named timeline is not visible to siblings so the animation stays
  // inactive and `#anim` remains transparent.
  assert_eq!(pixel(&pixmap_without_scope, 10, 10), (0, 0, 0, 255));

  let prepared_with_scope = renderer
    .prepare_html(&html_template(true), options)
    .expect("prepare with scope");
  let container_id =
    find_box_id_by_dom_id(&prepared_with_scope.box_tree().root, "container").expect("container");
  let container_frag =
    find_fragment_by_box_id(prepared_with_scope.fragment_tree(), container_id).expect("fragment");
  let container_style = container_frag.style.as_deref().expect("style");
  assert_eq!(
    container_style.timeline_scope,
    TimelineScopeProperty::Names(vec!["--scroller".to_string()])
  );
  let pixmap_with_scope = paint_at_bottom(&prepared_with_scope);
  // With promotion, the sibling animation is driven by the scroller's scroll position.
  assert_eq!(pixel(&pixmap_with_scope, 10, 10), (255, 0, 0, 255));
}

#[test]
fn timeline_scope_blocks_ancestor_timelines_inside_boundary() {
  let mut renderer = FastRender::new().expect("renderer");
  let options = RenderOptions::new().with_viewport(100, 100);

  let html_template = |with_scope: bool| {
    let scope_rule = if with_scope {
      "#inner { timeline-scope: --t; }"
    } else {
      ""
    };
    format!(
      r#"
      <style>
        html, body {{ margin: 0; background: rgb(0, 0, 0); }}
        #scroller {{
          overflow-y: scroll;
          height: 100px;
          width: 100px;
          scroll-timeline: --t block;
        }}
        {scope_rule}
        #anim {{
          width: 100px;
          height: 100px;
          background: rgb(255, 0, 0);
          opacity: 0;
          animation-timeline: --t;
          animation: fade auto linear;
        }}
        /* At scroll progress 0, this keyframe would make the element visible if the timeline is
           resolvable. */
        @keyframes fade {{ from {{ opacity: 1; }} to {{ opacity: 0; }} }}
      </style>
      <div id="scroller">
        <div id="inner"><div id="anim"></div></div>
        <div style="height: 200px;"></div>
      </div>
    "#
    )
  };

  let paint = |prepared: &fastrender::api::PreparedDocument| {
    let scroller_id =
      find_box_id_by_dom_id(&prepared.box_tree().root, "scroller").expect("scroller box_id");
    let scroll_state =
      ScrollState::from_parts(Point::ZERO, HashMap::from([(scroller_id, Point::ZERO)]));
    prepared
      .paint_with_options(
        PreparedPaintOptions::new()
          .with_scroll_state(scroll_state)
          .with_background(Rgba::new(0, 0, 0, 1.0)),
      )
      .expect("paint")
  };

  let prepared_without_scope = renderer
    .prepare_html(&html_template(false), options.clone())
    .expect("prepare without scope");
  let inner_id =
    find_box_id_by_dom_id(&prepared_without_scope.box_tree().root, "inner").expect("id");
  let inner_frag =
    find_fragment_by_box_id(prepared_without_scope.fragment_tree(), inner_id).expect("fragment");
  let inner_style = inner_frag.style.as_deref().expect("style");
  assert_eq!(inner_style.timeline_scope, TimelineScopeProperty::None);
  let pixmap_without_scope = paint(&prepared_without_scope);
  // Without a scope boundary, `#anim` resolves `--t` from the ancestor scroller.
  assert_eq!(pixel(&pixmap_without_scope, 10, 10), (255, 0, 0, 255));

  let prepared_with_scope = renderer
    .prepare_html(&html_template(true), options)
    .expect("prepare with scope");
  let inner_id = find_box_id_by_dom_id(&prepared_with_scope.box_tree().root, "inner").expect("id");
  let inner_frag =
    find_fragment_by_box_id(prepared_with_scope.fragment_tree(), inner_id).expect("fragment");
  let inner_style = inner_frag.style.as_deref().expect("style");
  assert_eq!(
    inner_style.timeline_scope,
    TimelineScopeProperty::Names(vec!["--t".to_string()])
  );
  let pixmap_with_scope = paint(&prepared_with_scope);
  // The boundary shadows `--t` with an inactive binding, so the ancestor timeline is not
  // resolvable and the keyframes have no effect.
  assert_eq!(pixel(&pixmap_with_scope, 10, 10), (0, 0, 0, 255));
}

#[test]
fn scroll_timeline_wins_over_view_timeline_with_same_name_on_element() {
  let mut renderer = FastRender::new().expect("renderer");
  let options = RenderOptions::new().with_viewport(100, 100);

  let html = r#"
    <style>
      html, body { margin: 0; background: rgb(0, 0, 0); }
      #scroller {
        overflow-y: scroll;
        height: 100px;
        width: 100px;
        background: rgb(255, 0, 0);
        opacity: 0;
        scroll-timeline: --x block;
        view-timeline: --x block;
        animation-timeline: --x;
        animation: fade auto linear;
      }
      @keyframes fade { from { opacity: 0; } to { opacity: 1; } }
    </style>
    <div id="scroller"><div style="height: 200px;"></div></div>
  "#;

  let prepared = renderer.prepare_html(html, options).expect("prepare");
  let scroller_id = find_box_id_by_dom_id(&prepared.box_tree().root, "scroller").expect("box_id");
  let scroller_frag =
    find_fragment_by_box_id(prepared.fragment_tree(), scroller_id).expect("fragment");
  let max_scroll =
    (scroller_frag.scroll_overflow.height() - scroller_frag.bounds.height()).max(0.0);
  assert!(max_scroll > 0.0, "expected scroll range for scroller");

  let paint = |scroll_y: f32| {
    let scroll_state = ScrollState::from_parts(
      Point::ZERO,
      HashMap::from([(scroller_id, Point::new(0.0, scroll_y))]),
    );
    prepared
      .paint_with_options(
        PreparedPaintOptions::new()
          .with_scroll_state(scroll_state)
          .with_background(Rgba::new(0, 0, 0, 1.0)),
      )
      .expect("paint")
  };

  let pixmap_top = paint(0.0);
  assert_eq!(pixel(&pixmap_top, 10, 10), (0, 0, 0, 255));

  let pixmap_bottom = paint(max_scroll);
  assert_eq!(pixel(&pixmap_bottom, 10, 10), (255, 0, 0, 255));
}

#[test]
fn timeline_scope_all_marks_duplicate_names_inactive() {
  let mut renderer = FastRender::new().expect("renderer");
  let options = RenderOptions::new().with_viewport(100, 100);

  let html = r#"
    <style>
      html, body { margin: 0; background: rgb(0, 0, 0); }
      #container { timeline-scope: all; }
      #anim {
        width: 100px;
        height: 50px;
        background: rgb(255, 0, 0);
        opacity: 0;
        animation-timeline: --dup;
        animation: fade auto linear;
      }
      .scroller {
        overflow-y: scroll;
        height: 25px;
        width: 100px;
        scroll-timeline: --dup block;
      }
      @keyframes fade { from { opacity: 0; } to { opacity: 1; } }
    </style>
    <div id="container">
      <div id="anim"></div>
      <div id="scroller1" class="scroller"><div style="height: 75px;"></div></div>
      <div id="scroller2" class="scroller"><div style="height: 75px;"></div></div>
    </div>
  "#;

  let prepared = renderer.prepare_html(html, options).expect("prepare");
  let scroller1_id = find_box_id_by_dom_id(&prepared.box_tree().root, "scroller1").expect("id");
  let scroller2_id = find_box_id_by_dom_id(&prepared.box_tree().root, "scroller2").expect("id");

  let scroller1_frag =
    find_fragment_by_box_id(prepared.fragment_tree(), scroller1_id).expect("fragment");
  let scroller2_frag =
    find_fragment_by_box_id(prepared.fragment_tree(), scroller2_id).expect("fragment");
  let max_scroll1 =
    (scroller1_frag.scroll_overflow.height() - scroller1_frag.bounds.height()).max(0.0);
  let max_scroll2 =
    (scroller2_frag.scroll_overflow.height() - scroller2_frag.bounds.height()).max(0.0);
  assert!(
    max_scroll1 > 0.0 && max_scroll2 > 0.0,
    "expected scroll ranges"
  );

  let scroll_state = ScrollState::from_parts(
    Point::ZERO,
    HashMap::from([
      (scroller1_id, Point::new(0.0, max_scroll1)),
      (scroller2_id, Point::new(0.0, max_scroll2)),
    ]),
  );
  let pixmap = prepared
    .paint_with_options(
      PreparedPaintOptions::new()
        .with_scroll_state(scroll_state)
        .with_background(Rgba::new(0, 0, 0, 1.0)),
    )
    .expect("paint");

  // The duplicate `--dup` name results in an inactive binding, so the animation stays inactive.
  assert_eq!(pixel(&pixmap, 10, 10), (0, 0, 0, 255));
}
