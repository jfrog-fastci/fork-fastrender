#![cfg(feature = "browser_ui")]

/// AccessKit integration helpers for winit-based front-ends.
///
/// FastRender already has a semantic accessibility tree (`crate::accessibility::AccessibilityNode`)
/// that is used by tests and by the `dump_a11y` CLI. When we surface that tree to the operating
/// system (e.g. for renderer-chrome/content accessibility), the winit layer will typically:
///
/// 1. build a FastRender accessibility tree (expensive),
/// 2. compute bounds for each node (expensive),
/// 3. build an `accesskit::TreeUpdate` (expensive),
/// 4. deliver it to the OS via `accesskit_winit::Adapter`.
///
/// `accesskit_winit::Adapter::update_if_active` will drop updates when assistive technology is not
/// connected. The closure-based API allows callers to keep expensive accessibility builders inside
/// the `update_if_active` callback so no work happens while AccessKit is inactive.

use accesskit::TreeUpdate;

/// Small abstraction over `accesskit_winit::Adapter` so we can unit test the active/inactive gate
/// without constructing a real winit window.
pub trait AccessKitAdapterLike {
  /// Deliver an update if AccessKit is active.
  fn update_if_active<F>(&self, update: F)
  where
    F: FnOnce() -> TreeUpdate;
}

impl AccessKitAdapterLike for accesskit_winit::Adapter {
  fn update_if_active<F>(&self, update: F)
  where
    F: FnOnce() -> TreeUpdate,
  {
    accesskit_winit::Adapter::update_if_active(self, update)
  }
}

/// Run the expensive accessibility builders only when AccessKit is active.
///
/// This is intended to be called from the winit integration layer's render/redraw path, right
/// before calling `Adapter::update_if_active`.
pub fn update_accesskit_if_active<A, Tree, Bounds, BuildTree, BuildBounds, BuildUpdate>(
  adapter: &A,
  build_tree: BuildTree,
  build_bounds: BuildBounds,
  build_update: BuildUpdate,
) where
  A: AccessKitAdapterLike,
  BuildTree: FnOnce() -> Tree,
  BuildBounds: FnOnce(&Tree) -> Bounds,
  BuildUpdate: FnOnce(Tree, Bounds) -> TreeUpdate,
{
  // Keep the expensive work inside the `update_if_active` closure. `accesskit_winit` will only
  // invoke this callback when assistive technology is connected.
  adapter.update_if_active(|| {
    let tree = build_tree();
    let bounds = build_bounds(&tree);
    build_update(tree, bounds)
  });
}

#[cfg(all(test, feature = "browser_ui"))]
mod tests {
  use super::*;
  use std::cell::Cell;

  #[derive(Default)]
  struct StubAdapter {
    active: bool,
    update_calls: Cell<usize>,
  }

  impl StubAdapter {
    fn new(active: bool) -> Self {
      Self {
        active,
        update_calls: Cell::new(0),
      }
    }
  }

  impl AccessKitAdapterLike for StubAdapter {
    fn update_if_active<F>(&self, update: F)
    where
      F: FnOnce() -> TreeUpdate,
    {
      if !self.active {
        return;
      }
      self.update_calls.set(self.update_calls.get() + 1);
      let _ = update();
    }
  }

  #[test]
  fn accesskit_update_path_is_gated_when_inactive() {
    let adapter = StubAdapter::new(false);

    let mut tree_built = 0usize;
    let mut bounds_built = 0usize;
    let mut update_built = 0usize;

    update_accesskit_if_active(
      &adapter,
      || {
        tree_built += 1;
        ()
      },
      |_| {
        bounds_built += 1;
        ()
      },
      |_, _| {
        update_built += 1;
        TreeUpdate::default()
      },
    );

    assert_eq!(
      adapter.update_calls.get(),
      0,
      "expected no adapter updates when inactive"
    );
    assert_eq!(tree_built, 0, "tree builder must not run when inactive");
    assert_eq!(bounds_built, 0, "bounds builder must not run when inactive");
    assert_eq!(update_built, 0, "TreeUpdate builder must not run when inactive");
  }

  #[test]
  fn accesskit_update_path_runs_when_active() {
    let adapter = StubAdapter::new(true);

    let mut tree_built = 0usize;
    let mut bounds_built = 0usize;
    let mut update_built = 0usize;

    update_accesskit_if_active(
      &adapter,
      || {
        tree_built += 1;
        ()
      },
      |_| {
        bounds_built += 1;
        ()
      },
      |_, _| {
        update_built += 1;
        TreeUpdate::default()
      },
    );

    assert_eq!(adapter.update_calls.get(), 1);
    assert_eq!(tree_built, 1);
    assert_eq!(bounds_built, 1);
    assert_eq!(update_built, 1);
  }
}
