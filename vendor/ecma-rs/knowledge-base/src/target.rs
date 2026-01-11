use semver::Version;

#[derive(
  Debug,
  Clone,
  Copy,
  PartialEq,
  Eq,
  Hash,
  serde::Serialize,
  serde::Deserialize,
)]
#[serde(rename_all = "lowercase")]
pub enum WebPlatform {
  Generic,
  Chrome,
  Firefox,
  Safari,
}

impl Default for WebPlatform {
  fn default() -> Self {
    Self::Generic
  }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TargetEnv {
  Node { version: Version },
  Web { platform: WebPlatform },
  /// No filtering by environment/version (conservative fallback).
  Unknown,
}
