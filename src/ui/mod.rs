pub mod cancel;
pub mod history;
pub mod messages;
pub mod worker;

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
