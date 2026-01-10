//! `ecma-rs/vm-js` backend.
//!
//! This backend is a thin adapter over FastRender's real `WindowHostState` + `EventLoop` runtime
//! (not a bespoke JS interpreter). The implementation lives in `backend_vmjs.rs` (historical naming).

#[allow(unused_imports)]
pub use crate::backend_vmjs::VmJsBackend;
