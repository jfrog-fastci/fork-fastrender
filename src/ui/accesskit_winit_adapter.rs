//! Winit ↔ AccessKit glue for the windowed browser UI.
//!
//! `egui` can generate [`accesskit::TreeUpdate`] values, but the platform-facing accessibility
//! adapter lives in [`accesskit_winit::Adapter`]. The adapter has two integration requirements:
//!
//! 1. **Event forwarding:** it must see certain `winit::event::WindowEvent`s (focus changes, resizes,
//!    etc). Today this is done by calling [`accesskit_winit::Adapter::on_event`] for each window
//!    event.
//! 2. **Tree updates:** the latest `TreeUpdate` emitted by egui must be forwarded to the adapter via
//!    [`accesskit_winit::Adapter::update_if_active`].
//!
//! The browser UI in `src/bin/browser.rs` uses this helper to keep the wiring explicit and to keep
//! unit tests headless-friendly (tests only compile the forwarding path; they do not create a real
//! window).
#![cfg(feature = "browser_ui")]

/// Wrapper around [`accesskit_winit::Adapter`].
///
/// The wrapper stores the adapter in an `Option` so unit tests can exercise the event-forwarding
/// entrypoint without instantiating a native window.
pub struct AccessKitWinitAdapter {
  adapter: Option<accesskit_winit::Adapter>,
}

impl AccessKitWinitAdapter {
  /// Construct a real AccessKit adapter for `window`.
  ///
  /// The adapter needs an initial tree update factory. For egui-based UIs, the recommended fallback
  /// is [`egui::Context::accesskit_placeholder_tree_update`], which provides a stable root node id
  /// matching egui's later updates.
  pub fn new<T: From<accesskit_winit::ActionRequestEvent> + Send + 'static>(
    window: &winit::window::Window,
    egui_ctx: &egui::Context,
    event_loop_proxy: winit::event_loop::EventLoopProxy<T>,
  ) -> Self {
    let egui_ctx = egui_ctx.clone();
    let adapter = accesskit_winit::Adapter::new(
      window,
      move || egui_ctx.accesskit_placeholder_tree_update(),
      event_loop_proxy,
    );
    Self {
      adapter: Some(adapter),
    }
  }

  /// Create a wrapper that does not attempt to talk to the platform accessibility APIs.
  #[must_use]
  pub fn disabled() -> Self {
    Self { adapter: None }
  }

  /// Create a wrapper that does not attempt to talk to the platform accessibility APIs.
  ///
  /// This is intended for unit tests that want to validate compile-time integration with winit
  /// event types without requiring an actual native windowing environment.
  #[must_use]
  pub fn disabled_for_test() -> Self {
    Self::disabled()
  }

  /// Forward a `winit` window event into AccessKit.
  ///
  /// Callers should invoke this for **every** `WindowEvent` associated with the window, including:
  /// - `Focused`
  /// - `Resized`
  /// - `ScaleFactorChanged`
  ///
  /// (On some platforms the adapter uses these events to keep the root window bounds and focus
  /// state in sync.)
  ///
  /// Returns the boolean result from [`accesskit_winit::Adapter::on_event`]. Callers should treat
  /// `true` as a signal to schedule a redraw so that a fresh accessibility tree update can be
  /// published (for example when assistive technology becomes active).
  ///
  /// For unit tests that don't have a real `Window`, pass `None` for `window` (the wrapper is a
  /// no-op when no adapter is configured).
  pub fn on_window_event(
    &self,
    window: Option<&winit::window::Window>,
    event: &winit::event::WindowEvent<'_>,
  ) -> bool {
    let (Some(adapter), Some(window)) = (self.adapter.as_ref(), window) else {
      return false;
    };
    adapter.on_event(window, event)
  }

  /// Forward the latest egui AccessKit tree update to the platform adapter.
  pub fn update(&self, update: accesskit::TreeUpdate) {
    let Some(adapter) = self.adapter.as_ref() else {
      return;
    };
    adapter.update_if_active(|| update);
  }
}

#[cfg(all(test, feature = "browser_ui"))]
mod tests {
  use super::AccessKitWinitAdapter;
  use winit::dpi::PhysicalSize;
  use winit::event::WindowEvent;

  #[test]
  fn accesskit_winit_event_forwarding_entrypoint_compiles_for_common_events() {
    // Compilation-level smoke test: unit tests run headless, so we cannot construct a real
    // `winit::window::Window` here. Instead we instantiate the adapter wrapper in its disabled form
    // and ensure the forwarding entrypoint accepts representative `WindowEvent`s.
    let adapter = AccessKitWinitAdapter::disabled_for_test();

    // Focus changes.
    let focused = WindowEvent::Focused(true);
    assert!(!adapter.on_window_event(None, &focused));

    // Resizes.
    let resized = WindowEvent::Resized(PhysicalSize::new(800, 600));
    assert!(!adapter.on_window_event(None, &resized));

    // DPI changes.
    let mut new_size = PhysicalSize::new(1024, 768);
    let scale = WindowEvent::ScaleFactorChanged {
      scale_factor: 2.0,
      new_inner_size: &mut new_size,
    };
    assert!(!adapter.on_window_event(None, &scale));
  }
}
