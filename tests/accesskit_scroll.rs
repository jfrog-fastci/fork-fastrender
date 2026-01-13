#![cfg(feature = "browser_ui")]

use std::path::PathBuf;
use std::sync::OnceLock;

use accesskit::{Action, ActionData, ActionRequest};
use fastrender::geometry::Size;
use fastrender::scroll::ScrollState;
use fastrender::{FastRender, FontConfig, RenderOptions};

fn deterministic_font_config() -> FontConfig {
  // Loading the full bundled fallback set is expensive; for integration tests we only need a small,
  // stable subset. Copy a few fixture fonts into a temporary directory and point the font loader at
  // it (mirrors `tests/browser_integration/support.rs`).
  static FONT_DIR: OnceLock<tempfile::TempDir> = OnceLock::new();

  let dir = FONT_DIR.get_or_init(|| {
    let dir = tempfile::tempdir().expect("temp font dir");
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let fonts = root.join("tests/fixtures/fonts");
    for name in [
      "NotoSans-subset.ttf",
      "NotoSerif-subset.ttf",
      "NotoSansMono-subset.ttf",
    ] {
      let src = fonts.join(name);
      let dst = dir.path().join(name);
      std::fs::copy(&src, &dst)
        .unwrap_or_else(|err| panic!("copy fixture font {}: {err}", src.display()));
    }
    dir
  });

  FontConfig::new()
    .with_system_fonts(false)
    .with_bundled_fonts(false)
    .with_font_dirs([dir.path().to_path_buf()])
}

fn deterministic_renderer() -> FastRender {
  FastRender::builder()
    .font_sources(deterministic_font_config())
    .build()
    .expect("build deterministic renderer")
}

fn rgba_at(pixmap: &fastrender::Pixmap, x: u32, y: u32) -> [u8; 4] {
  let width = pixmap.width();
  let height = pixmap.height();
  assert!(
    x < width && y < height,
    "rgba_at out of bounds: requested ({x}, {y}) in {width}x{height} pixmap"
  );
  let idx = (y as usize * width as usize + x as usize) * 4;
  let data = pixmap.data();
  [data[idx], data[idx + 1], data[idx + 2], data[idx + 3]]
}

fn prepare_doc(html: &str, viewport: (u32, u32)) -> (fastrender::PreparedDocument, Size) {
  let mut renderer = deterministic_renderer();
  let options = RenderOptions::new()
    .with_viewport(viewport.0, viewport.1)
    .with_device_pixel_ratio(1.0);
  let doc = renderer.prepare_html(html, options).expect("prepare html");
  (doc, Size::new(viewport.0 as f32, viewport.1 as f32))
}

#[test]
fn accesskit_scroll_properties_and_actions_for_viewport() {
  let html = r#"<!doctype html><html><head><style>
    body { margin: 0; }
    #top { width: 50px; height: 50px; background: rgb(255, 0, 0); }
    #bottom { width: 50px; height: 50px; background: rgb(0, 0, 255); }
  </style></head><body><div id="top"></div><div id="bottom"></div></body></html>"#;

  let (doc, viewport) = prepare_doc(html, (50, 50));
  let initial_scroll = ScrollState::default();

  let update = fastrender::accessibility_accesskit::build_scroll_container_tree_update(
    doc.fragment_tree(),
    viewport,
    &initial_scroll,
  );

  let root_id = update.tree.as_ref().expect("tree").root;
  assert_eq!(
    root_id,
    fastrender::accessibility_accesskit::ROOT_SCROLL_CONTAINER_ID
  );

  let root_node = update
    .nodes
    .iter()
    .find_map(|(id, node)| (*id == root_id).then_some(node))
    .expect("root node");

  assert_eq!(root_node.scroll_y().unwrap_or(0.0), 0.0);
  assert!(
    (root_node.scroll_y_max().unwrap_or(0.0) - 50.0).abs() < 0.5,
    "expected scroll_y_max≈50, got {:?}",
    root_node.scroll_y_max()
  );

  // Scroll the viewport down to reveal the second (blue) block.
  let req = ActionRequest {
    action: Action::SetScrollOffset,
    target: root_id,
    data: Some(ActionData::SetScrollOffset(accesskit::Point { x: 0.0, y: 50.0 })),
  };

  let next_scroll = fastrender::accessibility_accesskit::apply_scroll_action_to_scroll_state(
    doc.fragment_tree(),
    viewport,
    &initial_scroll,
    &req,
  )
  .expect("scroll action should apply");

  assert!((next_scroll.viewport.y - 50.0).abs() < 0.5);

  // The updated scroll state should be reflected back into the AccessKit node.
  let update_after = fastrender::accessibility_accesskit::build_scroll_container_tree_update(
    doc.fragment_tree(),
    viewport,
    &next_scroll,
  );
  let root_after = update_after
    .nodes
    .iter()
    .find_map(|(id, node)| (*id == root_id).then_some(node))
    .expect("root node after scroll");
  assert!((root_after.scroll_y().unwrap_or(0.0) - 50.0).abs() < 0.5);

  let pixmap = doc
    .paint_with_scroll_state(next_scroll, None, None, None)
    .expect("paint");
  assert_eq!(rgba_at(&pixmap, 10, 10), [0, 0, 255, 255]);
}

#[test]
fn accesskit_scroll_properties_and_actions_for_element_scroll_container() {
  let html = r#"<!doctype html><html><head><style>
    body { margin: 0; }
    #scroller { width: 50px; height: 50px; overflow: auto; }
    .item { width: 50px; height: 50px; }
    #red { background: rgb(255, 0, 0); }
    #blue { background: rgb(0, 0, 255); }
  </style></head><body><div id="scroller"><div id="red" class="item"></div><div id="blue" class="item"></div></div></body></html>"#;

  let (doc, viewport) = prepare_doc(html, (50, 50));
  let initial_scroll = ScrollState::default();

  let update = fastrender::accessibility_accesskit::build_scroll_container_tree_update(
    doc.fragment_tree(),
    viewport,
    &initial_scroll,
  );

  // Find the element scroll container node by excluding the root and looking for a non-zero max.
  let (container_id, container_node) = update
    .nodes
    .iter()
    .find_map(|(id, node)| {
      if *id == fastrender::accessibility_accesskit::ROOT_SCROLL_CONTAINER_ID {
        return None;
      }
      node
        .scroll_y_max()
        .and_then(|max| (max > 0.0).then_some((*id, node)))
    })
    .expect("expected one element scroll container node");

  assert_eq!(container_node.scroll_y().unwrap_or(0.0), 0.0);
  assert!(
    (container_node.scroll_y_max().unwrap_or(0.0) - 50.0).abs() < 0.5,
    "expected scroll_y_max≈50, got {:?}",
    container_node.scroll_y_max()
  );

  // Scroll the element container down to reveal the second (blue) child.
  let req = ActionRequest {
    action: Action::SetScrollOffset,
    target: container_id,
    data: Some(ActionData::SetScrollOffset(accesskit::Point { x: 0.0, y: 50.0 })),
  };

  let next_scroll = fastrender::accessibility_accesskit::apply_scroll_action_to_scroll_state(
    doc.fragment_tree(),
    viewport,
    &initial_scroll,
    &req,
  )
  .expect("scroll action should apply");

  assert!(
    next_scroll
      .elements
      .values()
      .any(|p| (p.y - 50.0).abs() < 0.5),
    "expected element scroll to be updated: {next_scroll:?}"
  );

  // The AccessKit node should reflect the updated element scroll offset.
  let update_after = fastrender::accessibility_accesskit::build_scroll_container_tree_update(
    doc.fragment_tree(),
    viewport,
    &next_scroll,
  );
  let container_after = update_after
    .nodes
    .iter()
    .find_map(|(id, node)| (*id == container_id).then_some(node))
    .expect("container node after scroll");
  assert!((container_after.scroll_y().unwrap_or(0.0) - 50.0).abs() < 0.5);

  let pixmap = doc
    .paint_with_scroll_state(next_scroll, None, None, None)
    .expect("paint");
  assert_eq!(rgba_at(&pixmap, 10, 10), [0, 0, 255, 255]);
}

