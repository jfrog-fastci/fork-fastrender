pub mod about_pages;
pub mod browser_app;
pub mod browser_worker;
pub mod cancel;
pub mod chrome;
pub mod history;
pub mod messages;
pub mod worker;
pub mod worker_loop;

pub use messages::{
  NavigationReason, PointerButton, RenderedFrame, RepaintReason, TabId, UiToWorker, WorkerToUi,
};

// `input_mapping` depends on the optional egui/winit stack, so keep it behind the
// `browser_ui` feature gate.
#[cfg(feature = "browser_ui")]
pub mod input_mapping;

#[cfg(feature = "browser_ui")]
pub use input_mapping::{InputMapping, WheelDelta, CSS_PX_PER_WHEEL_LINE};

// `pixmap_texture` depends on the optional egui stack, so keep it behind the
// `browser_ui` feature gate.
#[cfg(feature = "browser_ui")]
pub mod pixmap_texture;

#[cfg(feature = "browser_ui")]
pub use pixmap_texture::PageTexture;

#[cfg(feature = "browser_ui")]
pub mod wgpu_pixmap_texture;

#[cfg(feature = "browser_ui")]
pub use wgpu_pixmap_texture::WgpuPixmapTexture;

#[cfg(feature = "browser_ui")]
pub mod url;

#[cfg(feature = "browser_ui")]
pub use url::normalize_user_url;

pub use history::{HistoryEntry, TabHistory};

pub use browser_app::{BrowserAppState, BrowserTabState, ChromeState, LatestFrameMeta};
pub use chrome::{chrome_ui, ChromeAction};
