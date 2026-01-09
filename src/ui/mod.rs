pub mod about_pages;
pub mod browser_app;
pub mod browser_tab_controller;
// UI↔worker messaging lives in `messages.rs`.
//
// `render_worker` is the *single* production UI worker implementation. The `browser` binary and
// browser integration tests are expected to use it.
//
// Ownership contracts:
// - The worker owns per-tab navigation history. The UI drives it via `UiToWorker::{GoBack,GoForward,Reload}`.
// - Cancellation is cooperative: the UI should retain the per-tab `CancelGens` from `CreateTab` and
//   bump generations before sending new actions so in-flight work can be ignored/cancelled.
//
// `worker` contains small render-thread utilities (stage heartbeat forwarding, thread builder), but
// does **not** implement a separate UI worker loop.
pub mod render_worker;
pub mod cancel;
pub mod history;
pub mod messages;
pub mod shortcuts;
pub mod worker;
pub mod url;

// `chrome` depends on egui, so keep it behind the `browser_ui` feature gate.
#[cfg(feature = "browser_ui")]
pub mod chrome;

pub use messages::{
  NavigationReason, PointerButton, RenderedFrame, RepaintReason, TabId, UiToWorker, WorkerToUi,
};

// `input_mapping` depends on the optional egui/winit stack, so keep it behind the
// `browser_ui` feature gate.
#[cfg(feature = "browser_ui")]
pub mod input_mapping;

#[cfg(feature = "browser_ui")]
pub use input_mapping::{InputMapping, WheelDelta, CSS_PX_PER_WHEEL_LINE};

pub use browser_tab_controller::BrowserTabController;
pub use render_worker::{
  spawn_browser_ui_worker, spawn_browser_worker, spawn_browser_worker_with_factory, spawn_ui_worker,
  spawn_test_browser_worker, spawn_ui_worker_for_test, spawn_ui_worker_with_factory,
  BrowserWorkerHandle, UiThreadWorkerHandle,
};
#[cfg(any(test, feature = "browser_ui"))]
pub use render_worker::spawn_browser_worker_for_test;

#[cfg(feature = "browser_ui")]
pub mod wgpu_pixmap_texture;

#[cfg(feature = "browser_ui")]
pub use wgpu_pixmap_texture::WgpuPixmapTexture;
pub use url::normalize_user_url;

pub use history::{HistoryEntry, TabHistory};
pub use browser_app::{
  AppUpdate, BrowserAppState, BrowserTabState, ChromeState, FrameReadyUpdate, LatestFrameMeta,
  OpenSelectDropdownUpdate,
};

#[cfg(feature = "browser_ui")]
pub use chrome::{chrome_ui, ChromeAction};
pub use crate::select_dropdown as select_dropdown;
pub use crate::select_dropdown::{SelectDropdown, SelectDropdownChoice};
