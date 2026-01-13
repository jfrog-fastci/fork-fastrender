//! Site isolation primitives used by multiprocess/browser plumbing.
//!
//! This is a directory module (`src/site_isolation/`). Keep it that way: adding a sibling
//! `src/site_isolation.rs` file will create a Rust module ambiguity (`E0761`).

pub mod policy;
pub mod site_key;

pub use policy::{should_isolate_child_frame, SiteIsolationMode};
pub use site_key::{
  site_key_for_navigation, FileUrlSiteIsolation, OriginKey, SiteKey, SiteKeyFactory,
};
