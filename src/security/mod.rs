//! Security-related helpers and sandboxing infrastructure.
//!
//! Most of FastRender is currently single-process. This module contains
//! utilities that are intended to be reused by the future multiprocess
//! renderer sandbox.

pub mod macos_renderer_sandbox;
