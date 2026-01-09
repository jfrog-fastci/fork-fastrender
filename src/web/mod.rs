//! Web platform APIs.
//!
//! This module is separate from the renderer DOM (`crate::dom` / `crate::dom2`) so we can build
//! spec-shaped Web APIs without polluting renderer-centric data structures.

pub mod dom;
pub mod events;
