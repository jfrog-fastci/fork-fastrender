use serde::{Deserialize, Serialize};

/// Parsed payload emitted by `resources/fastrender_testharness_report.js`.
///
/// This is intentionally a simple, JSON-friendly representation so the report can be embedded
/// verbatim in higher-level suite reports.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WptReport {
  pub file_status: String,
  pub harness_status: String,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub message: Option<String>,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub stack: Option<String>,
  #[serde(default)]
  pub subtests: Vec<WptSubtest>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WptSubtest {
  pub name: String,
  pub status: String,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub message: Option<String>,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub stack: Option<String>,
}
