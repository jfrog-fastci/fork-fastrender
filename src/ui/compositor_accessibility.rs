#![cfg(feature = "browser_ui_base")]

use accesskit::{Action, NodeBuilder, NodeClassSet, NodeId, Rect, Role, Tree, TreeUpdate};
use std::num::NonZeroU128;

use crate::ui::messages::ScrollMetrics;

/// Stable node ids for the compositor (non-egui) browser UI accessibility tree.
///
/// These IDs must remain stable across updates so assistive technology does not "lose" the tree.
///
/// Note: `NodeId` wraps a `NonZeroU128` so `0` is reserved/invalid.
pub fn root_node_id() -> NodeId {
  NodeId(NonZeroU128::new(1).expect("nonzero")) // fastrender-allow-unwrap
}

pub fn chrome_node_id() -> NodeId {
  NodeId(NonZeroU128::new(2).expect("nonzero")) // fastrender-allow-unwrap
}

pub fn page_node_id() -> NodeId {
  NodeId(NonZeroU128::new(3).expect("nonzero")) // fastrender-allow-unwrap
}

pub const DEFAULT_WINDOW_NAME: &str = "FastRender Browser";
pub const DEFAULT_CHROME_NAME: &str = "Browser chrome";
pub const DEFAULT_PAGE_NAME: &str = "Web page content";

/// Minimal AccessKit integration for the compositor (non-egui) browser UI backend.
///
/// The egui UI uses `egui-winit`'s built-in AccessKit integration; however, when running a custom
/// compositor (no egui), we must install our own `accesskit_winit::Adapter` and provide a minimal
/// tree so screen readers can at least discover the top-level chrome + page regions.
pub struct CompositorAccessibility {
  adapter: accesskit_winit::Adapter,
}

impl CompositorAccessibility {
  /// Create a new adapter for the given window.
  ///
  /// The provided `event_loop_proxy` is used by `accesskit_winit` to forward accessibility action
  /// requests (e.g. focus changes) into the winit event loop as user events.
  pub fn new<T>(
    window: &winit::window::Window,
    event_loop_proxy: winit::event_loop::EventLoopProxy<T>,
    initial_state: CompositorA11yState,
  ) -> Self
  where
    T: From<accesskit_winit::ActionRequestEvent> + Send + 'static,
  {
    let adapter = accesskit_winit::Adapter::new(
      window,
      move || build_initial_tree_update(&initial_state),
      event_loop_proxy,
    );

    Self { adapter }
  }

  /// Forward a winit `WindowEvent` to the AccessKit adapter.
  pub fn on_window_event(
    &self,
    window: &winit::window::Window,
    event: &winit::event::WindowEvent<'_>,
  ) -> bool {
    self.adapter.on_event(window, event)
  }

  /// Update the minimal accessibility tree.
  ///
  /// This should be called whenever:
  /// - the window is resized or its scale factor changes (bounds change),
  /// - the UI focus changes between chrome and page,
  /// - the active tab changes (page node name changes).
  pub fn update_if_active(&self, state: &CompositorA11yState) {
    self.adapter.update_if_active(|| build_tree_update(state));
  }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompositorFocusTarget {
  Chrome,
  Page,
}

/// Inputs required to build the minimal compositor AccessKit tree.
#[derive(Debug, Clone)]
pub struct CompositorA11yState {
  /// Accessible name for the window/root node.
  pub window_name: String,
  pub chrome_name: String,
  pub page_name: String,
  pub window_bounds: Rect,
  pub chrome_bounds: Rect,
  pub page_bounds: Rect,
  /// Optional scroll metrics for the active page.
  ///
  /// When present, the page node exposes `scroll_x/y` properties and supports scroll-related
  /// actions (so assistive tech can scroll the rendered page region even when the underlying page
  /// content is a bitmap).
  pub page_scroll: Option<ScrollMetrics>,
  pub focus: CompositorFocusTarget,
}

/// Convenience helper for building [`CompositorA11yState`] from a winit window and a fixed chrome
/// height (in physical pixels).
pub fn state_for_window(
  window: &winit::window::Window,
  chrome_height_px: f64,
  window_name: impl Into<String>,
  chrome_name: impl Into<String>,
  page_name: impl Into<String>,
  page_scroll: Option<ScrollMetrics>,
  focus: CompositorFocusTarget,
) -> CompositorA11yState {
  let size = window.inner_size();
  let width = size.width as f64;
  let height = size.height as f64;

  let chrome_height_px = chrome_height_px.clamp(0.0, height.max(0.0));

  CompositorA11yState {
    window_name: window_name.into(),
    chrome_name: chrome_name.into(),
    page_name: page_name.into(),
    window_bounds: Rect {
      x0: 0.0,
      y0: 0.0,
      x1: width,
      y1: height,
    },
    chrome_bounds: Rect {
      x0: 0.0,
      y0: 0.0,
      x1: width,
      y1: chrome_height_px,
    },
    page_bounds: Rect {
      x0: 0.0,
      y0: chrome_height_px,
      x1: width,
      y1: height,
    },
    page_scroll,
    focus,
  }
}

fn set_scroll_properties_for_page(builder: &mut NodeBuilder, metrics: ScrollMetrics) {
  // AccessKit scroll offsets/bounds are `f64`s. Keep the values finite and non-negative.
  let sanitize_axis = |v: f32| -> f64 {
    if v.is_finite() {
      v.max(0.0) as f64
    } else {
      0.0
    }
  };

  builder.set_scroll_x(sanitize_axis(metrics.scroll_css.0));
  builder.set_scroll_y(sanitize_axis(metrics.scroll_css.1));
  builder.set_scroll_x_min(sanitize_axis(metrics.bounds_css.min_x));
  builder.set_scroll_y_min(sanitize_axis(metrics.bounds_css.min_y));
  builder.set_scroll_x_max(sanitize_axis(metrics.bounds_css.max_x));
  builder.set_scroll_y_max(sanitize_axis(metrics.bounds_css.max_y));

  // Only advertise scroll actions when the scroll range is non-zero; this avoids telling screen
  // readers that a non-scrollable page can be scrolled.
  let scrollable_x = metrics.bounds_css.max_x > metrics.bounds_css.min_x;
  let scrollable_y = metrics.bounds_css.max_y > metrics.bounds_css.min_y;
  if !(scrollable_x || scrollable_y) {
    return;
  }

  for action in [
    Action::ScrollUp,
    Action::ScrollDown,
    Action::ScrollLeft,
    Action::ScrollRight,
    Action::ScrollForward,
    Action::ScrollBackward,
    Action::ScrollToPoint,
    Action::SetScrollOffset,
  ] {
    builder.add_action(action);
  }
}

fn build_initial_tree_update(state: &CompositorA11yState) -> TreeUpdate {
  let mut classes = NodeClassSet::new();

  let mut root = NodeBuilder::new(Role::Window);
  root.set_name(state.window_name.clone());
  root.set_bounds(state.window_bounds);
  root.set_children(vec![chrome_node_id(), page_node_id()]);

  let mut chrome = NodeBuilder::new(Role::Group);
  chrome.set_name(state.chrome_name.clone());
  chrome.set_bounds(state.chrome_bounds);
  chrome.add_action(Action::Focus);

  let mut page = NodeBuilder::new(Role::WebView);
  page.set_name(state.page_name.clone());
  page.set_bounds(state.page_bounds);
  page.add_action(Action::Focus);
  if let Some(scroll) = state.page_scroll {
    set_scroll_properties_for_page(&mut page, scroll);
  }

  let focus_id = match state.focus {
    CompositorFocusTarget::Chrome => chrome_node_id(),
    CompositorFocusTarget::Page => page_node_id(),
  };

  TreeUpdate {
    nodes: vec![
      (root_node_id(), root.build(&mut classes)),
      (chrome_node_id(), chrome.build(&mut classes)),
      (page_node_id(), page.build(&mut classes)),
    ],
    tree: Some(Tree::new(root_node_id())),
    focus: Some(focus_id),
  }
}

fn build_tree_update(state: &CompositorA11yState) -> TreeUpdate {
  let mut update = build_initial_tree_update(state);
  // The root tree is only provided when the adapter becomes active. Subsequent updates should omit
  // `tree` so assistive technology sees stable node identities rather than a "fresh" tree.
  update.tree = None;
  update
}

/// Determine which region should receive focus for an incoming AccessKit action request.
pub fn focus_target_from_action_request(
  event: &accesskit_winit::ActionRequestEvent,
) -> Option<CompositorFocusTarget> {
  let request = &event.request;
  if request.action != Action::Focus {
    return None;
  }
  if request.target == page_node_id() {
    Some(CompositorFocusTarget::Page)
  } else if request.target == chrome_node_id() {
    Some(CompositorFocusTarget::Chrome)
  } else {
    None
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn build_tree_update_contains_stable_nodes_and_names() {
    let state = CompositorA11yState {
      window_name: "FastRender Browser".to_string(),
      chrome_name: "Browser chrome".to_string(),
      page_name: "Web page content".to_string(),
      window_bounds: Rect {
        x0: 0.0,
        y0: 0.0,
        x1: 800.0,
        y1: 600.0,
      },
      chrome_bounds: Rect {
        x0: 0.0,
        y0: 0.0,
        x1: 800.0,
        y1: 80.0,
      },
      page_bounds: Rect {
        x0: 0.0,
        y0: 80.0,
        x1: 800.0,
        y1: 600.0,
      },
      page_scroll: None,
      focus: CompositorFocusTarget::Page,
    };

    let initial = build_initial_tree_update(&state);
    assert_eq!(initial.focus, Some(page_node_id()));
    assert_eq!(initial.tree.as_ref().map(|t| t.root), Some(root_node_id()));

    let mut names: Vec<(NodeId, String)> = initial
      .nodes
      .iter()
      .map(|(id, node)| (*id, node.name().unwrap_or("").to_string()))
      .collect();
    names.sort_by_key(|(id, _)| id.0.get());

    assert_eq!(
      names,
      vec![
        (root_node_id(), "FastRender Browser".to_string()),
        (chrome_node_id(), "Browser chrome".to_string()),
        (page_node_id(), "Web page content".to_string()),
      ]
    );

    let mut roles: Vec<(NodeId, String)> = initial
      .nodes
      .iter()
      .map(|(id, node)| (*id, format!("{:?}", node.role())))
      .collect();
    roles.sort_by_key(|(id, _)| id.0.get());
    assert_eq!(
      roles,
      vec![
        (root_node_id(), "Window".to_string()),
        (chrome_node_id(), "Group".to_string()),
        (page_node_id(), "WebView".to_string()),
      ]
    );

    let update = build_tree_update(&state);
    assert_eq!(update.tree, None);
  }

  #[test]
  fn page_node_exposes_scroll_properties_when_metrics_present() {
    let state = CompositorA11yState {
      window_name: "FastRender Browser".to_string(),
      chrome_name: "Browser chrome".to_string(),
      page_name: "Web page content".to_string(),
      window_bounds: Rect {
        x0: 0.0,
        y0: 0.0,
        x1: 800.0,
        y1: 600.0,
      },
      chrome_bounds: Rect {
        x0: 0.0,
        y0: 0.0,
        x1: 800.0,
        y1: 80.0,
      },
      page_bounds: Rect {
        x0: 0.0,
        y0: 80.0,
        x1: 800.0,
        y1: 600.0,
      },
      page_scroll: Some(ScrollMetrics {
        viewport_css: (400, 300),
        scroll_css: (0.0, 120.0),
        bounds_css: crate::scroll::ScrollBounds {
          min_x: 0.0,
          min_y: 0.0,
          max_x: 0.0,
          max_y: 900.0,
        },
        content_css: (400.0, 1200.0),
      }),
      focus: CompositorFocusTarget::Page,
    };

    let update = build_initial_tree_update(&state);
    let page = update
      .nodes
      .iter()
      .find_map(|(id, node)| (*id == page_node_id()).then_some(node))
      .expect("page node missing");

    assert_eq!(page.scroll_y().unwrap_or(0.0), 120.0);
    assert_eq!(page.scroll_y_max().unwrap_or(0.0), 900.0);
    assert!(
      page
        .actions()
        .iter()
        .any(|a| *a == Action::SetScrollOffset),
      "expected scroll actions to be advertised when scroll range is non-zero"
    );
  }
}
