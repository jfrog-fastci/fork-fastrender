//! UI-facing re-export of [`crate::string_match`].
//!
//! Many UI modules historically referenced `super::string_match::*`. Keep that stable internal
//! module path while sharing the canonical implementation in `src/string_match.rs` (usable by
//! non-UI code too).

pub(crate) use crate::string_match::{
  contains_ascii_case_insensitive, find_ascii_case_insensitive, AsciiCaseInsensitive,
};
