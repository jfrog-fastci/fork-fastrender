pub mod cancel;
pub mod network;
pub mod renderer;

// Compatibility re-exports: the transport layer historically referred to the top-level
// `ipc::protocol::{BrowserToRenderer, RendererToBrowser}` types. Keep those paths stable while the
// protocol is split into submodules.
pub use renderer::{BrowserToRenderer, RendererToBrowser};
