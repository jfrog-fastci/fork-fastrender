//! Fallible, size-bounded URL / URLSearchParams core types.
//!
//! These types are intended for hostile input (JavaScript bindings, arbitrary URL strings).
//! All allocations are guarded (fallible + bounded) to avoid abort-on-OOM.

mod error;
mod limits;
mod search_params;
mod url;

pub use error::{WebUrlError, WebUrlLimitKind};
pub use limits::WebUrlLimits;
pub use search_params::WebUrlSearchParams;
pub use url::WebUrl;

