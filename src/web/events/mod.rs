//! Web-exposed re-exports of the canonical DOM Events foundation.
//!
//! FastRender's event dispatch foundation lives in [`crate::dom2::events`]. Historically we also had
//! a standalone `web::events` implementation; that duplicated the dispatch algorithm and drifted.
//!
//! This module now exists only as a compatibility path for "web-ish" code. All behavior is defined
//! by `dom2::events`.

pub use crate::dom2::events::*;

/// `dom2::events` currently uses a single options struct (`EventListenerOptions`) for both
/// `addEventListener` and `removeEventListener`.
///
/// The DOM/Web IDL spec names the dictionary accepted by `addEventListener` as
/// `AddEventListenerOptions`, so provide this alias for web-facing code.
pub type AddEventListenerOptions = EventListenerOptions;

#[cfg(test)]
mod tests;
