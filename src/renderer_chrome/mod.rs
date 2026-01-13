//! Infrastructure for the "renderer chrome" workstream (browser UI rendered by FastRender itself).
//!
//! This module is currently small and focused on bridging accessibility action requests into
//! trusted DOM events so chrome HTML/JS can respond the same way it would to pointer input.

pub mod accesskit_actions;

