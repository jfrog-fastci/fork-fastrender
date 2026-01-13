//! Runtime configuration for the renderer process-assignment policy.
//!
//! This module is intentionally kept independent of the optional `browser_ui` stack (winit/wgpu/egui)
//! so it can be used by headless tests and CLI tools.
//!
//! # Environment variables
//!
//! - `FASTR_PROCESS_MODEL` selects how tabs/origins are mapped to renderer processes.
//!   - `tab` (default): one renderer process per tab.
//!   - `site` / `origin`: one renderer process per site key (origin).
//!
//! Unset / empty values fall back to the default (`tab`).

use crate::ui::process_assignment::ProcessModel;

/// Environment variable that selects the process assignment model.
pub const ENV_PROCESS_MODEL: &str = "FASTR_PROCESS_MODEL";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessModelParseError {
  raw: String,
}

impl std::fmt::Display for ProcessModelParseError {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    write!(
      f,
      "invalid process model {:?} (expected: tab | site | origin)",
      self.raw
    )
  }
}

impl std::error::Error for ProcessModelParseError {}

/// Parse a process assignment model from a raw string.
///
/// - `None` / empty string => default (`tab`)
/// - Accepted values: `tab`, `site`, `origin` (case-insensitive, ASCII whitespace trimmed)
pub fn parse_process_model(raw: Option<&str>) -> Result<ProcessModel, ProcessModelParseError> {
  let Some(raw) = raw else {
    return Ok(ProcessModel::default());
  };
  let trimmed = raw.trim();
  if trimmed.is_empty() {
    return Ok(ProcessModel::default());
  }

  match trimmed.to_ascii_lowercase().as_str() {
    "tab" => Ok(ProcessModel::PerTab),
    "site" | "origin" => Ok(ProcessModel::PerSiteKey),
    _ => Err(ProcessModelParseError {
      raw: trimmed.to_string(),
    }),
  }
}

/// Resolve the process assignment model from an env var value, falling back to the default.
///
/// If the value is invalid, the error is logged to stderr and the default is returned.
pub fn process_model_from_env_value(raw: Option<&str>) -> ProcessModel {
  match parse_process_model(raw) {
    Ok(model) => model,
    Err(err) => {
      eprintln!(
        "{ENV_PROCESS_MODEL}={:?}: {err}; falling back to {:?}",
        raw.unwrap_or_default(),
        ProcessModel::default(),
      );
      ProcessModel::default()
    }
  }
}

/// Read [`ENV_PROCESS_MODEL`] and return the configured process assignment model.
///
/// Invalid values are logged to stderr and treated as the default (`tab`).
pub fn process_model_from_env() -> ProcessModel {
  let raw = std::env::var(ENV_PROCESS_MODEL).ok();
  process_model_from_env_value(raw.as_deref())
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn parse_process_assignment_model_defaults_to_tab() {
    assert_eq!(parse_process_model(None).unwrap(), ProcessModel::PerTab);
    assert_eq!(
      parse_process_model(Some("")).unwrap(),
      ProcessModel::PerTab
    );
    assert_eq!(
      parse_process_model(Some("   \n\t")).unwrap(),
      ProcessModel::PerTab
    );
  }

  #[test]
  fn parse_process_assignment_model_tab_variants() {
    assert_eq!(
      parse_process_model(Some("tab")).unwrap(),
      ProcessModel::PerTab
    );
    assert_eq!(
      parse_process_model(Some("TAB")).unwrap(),
      ProcessModel::PerTab
    );
    assert_eq!(
      parse_process_model(Some("  tab  ")).unwrap(),
      ProcessModel::PerTab
    );
  }

  #[test]
  fn parse_process_assignment_model_site_variants() {
    assert_eq!(
      parse_process_model(Some("site")).unwrap(),
      ProcessModel::PerSiteKey
    );
    assert_eq!(
      parse_process_model(Some("origin")).unwrap(),
      ProcessModel::PerSiteKey
    );
    assert_eq!(
      parse_process_model(Some("ORIGIN")).unwrap(),
      ProcessModel::PerSiteKey
    );
  }

  #[test]
  fn parse_process_assignment_model_rejects_invalid_values() {
    assert!(parse_process_model(Some("invalid")).is_err());
    assert!(parse_process_model(Some("tabs")).is_err());
    assert!(parse_process_model(Some("sitekey")).is_err());
  }

  #[test]
  fn env_value_helper_falls_back_to_default() {
    assert_eq!(
      process_model_from_env_value(Some("invalid")),
      ProcessModel::PerTab
    );
  }
}
