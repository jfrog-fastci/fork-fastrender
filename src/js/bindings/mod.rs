//! WebIDL-driven JavaScript bindings.
//!
//! The generated glue lives under [`generated`]. The host boundary is defined in [`host`].

pub mod generated;
pub mod host;

pub use generated::install_window_bindings;
pub use host::{BindingValue, WebHostBindings};

