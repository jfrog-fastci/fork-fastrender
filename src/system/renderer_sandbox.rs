//! Renderer sandbox configuration parsed from environment variables.
//!
//! This module intentionally keeps the parsing logic dependency-free (std only) so it can be reused
//! by binaries and test harnesses without pulling in additional crates.
//!
//! The sandbox itself is applied elsewhere (e.g. a renderer process entrypoint). This module
//! focuses purely on *configuration plumbing* so higher-level code can decide enforcement policy
//! (fail-open vs fail-closed) while still allowing developers to opt out locally when diagnosing
//! sandbox issues.

use std::error::Error;
use std::fmt;

pub const ENV_DISABLE_RENDERER_SANDBOX: &str = "FASTR_DISABLE_RENDERER_SANDBOX";
pub const ENV_RENDERER_SECCOMP: &str = "FASTR_RENDERER_SECCOMP";
pub const ENV_RENDERER_LANDLOCK: &str = "FASTR_RENDERER_LANDLOCK";
pub const ENV_RENDERER_CLOSE_FDS: &str = "FASTR_RENDERER_CLOSE_FDS";

/// Effective runtime configuration for the renderer sandbox.
///
/// Defaults should be chosen by higher-level code (production vs tests). This type represents the
/// *final* runtime decisions after defaults + environment overrides have been applied.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RendererSandboxConfig {
  /// Whether sandboxing should be attempted at all.
  pub enabled: bool,
  /// Whether to apply a seccomp-bpf syscall filter (Linux-only).
  pub seccomp: bool,
  /// Whether to apply a Landlock filesystem sandbox (Linux-only).
  pub landlock: bool,
  /// Whether to close non-stdio file descriptors at renderer startup.
  pub close_fds: bool,
}

impl RendererSandboxConfig {
  /// Defaults for renderer process - sandboxing is disabled.
  ///
  /// Sandboxing is not currently used in FastRender.
  pub const fn production_defaults() -> Self {
    Self {
      enabled: false,
      seccomp: false,
      landlock: false,
      close_fds: false,
    }
  }

  /// Parse sandbox configuration from the current process environment, using
  /// [`RendererSandboxConfig::production_defaults`] as the baseline.
  pub fn from_env() -> Result<Self, SandboxEnvError> {
    Self::from_env_with_defaults(Self::production_defaults())
  }

  /// Parse sandbox configuration from the current process environment, applying overrides on top
  /// of the provided defaults.
  pub fn from_env_with_defaults(defaults: Self) -> Result<Self, SandboxEnvError> {
    Self::from_lookup_with_defaults(|name| std::env::var(name).ok(), defaults)
  }

  /// Parse sandbox configuration from an arbitrary key/value source.
  ///
  /// This is primarily intended for unit tests and callers that already have an env map.
  pub fn from_lookup_with_defaults(
    mut lookup: impl FnMut(&str) -> Option<String>,
    defaults: Self,
  ) -> Result<Self, SandboxEnvError> {
    let disable_sandbox = parse_optional_disable_flag(lookup(ENV_DISABLE_RENDERER_SANDBOX));
    let seccomp_override = parse_optional_bool(ENV_RENDERER_SECCOMP, lookup(ENV_RENDERER_SECCOMP))?;
    let landlock_override =
      parse_optional_bool(ENV_RENDERER_LANDLOCK, lookup(ENV_RENDERER_LANDLOCK))?;
    let close_fds_override =
      parse_optional_bool(ENV_RENDERER_CLOSE_FDS, lookup(ENV_RENDERER_CLOSE_FDS))?;

    // Start from the provided defaults and apply per-var overrides.
    let mut config = defaults;

    if disable_sandbox == Some(true) {
      config.enabled = false;
    }

    if !config.enabled {
      // When the sandbox is disabled, treat all layers as disabled regardless of per-layer toggles.
      config.seccomp = false;
      config.landlock = false;
      config.close_fds = false;
      return Ok(config);
    }

    if let Some(seccomp) = seccomp_override {
      config.seccomp = seccomp;
    }
    if let Some(landlock) = landlock_override {
      config.landlock = landlock;
    }
    if let Some(close_fds) = close_fds_override {
      config.close_fds = close_fds;
    }

    Ok(config)
  }
}

impl Default for RendererSandboxConfig {
  fn default() -> Self {
    Self::production_defaults()
  }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SandboxEnvError {
  var: &'static str,
  value: String,
}

impl SandboxEnvError {
  fn new(var: &'static str, value: String) -> Self {
    Self { var, value }
  }

  pub fn var(&self) -> &'static str {
    self.var
  }

  pub fn value(&self) -> &str {
    &self.value
  }
}

impl fmt::Display for SandboxEnvError {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    write!(
      f,
      "{}: invalid value {:?}; expected 0|1|true|false|yes|no|on|off",
      self.var, self.value
    )
  }
}

impl Error for SandboxEnvError {}

fn parse_optional_bool(
  var: &'static str,
  raw: Option<String>,
) -> Result<Option<bool>, SandboxEnvError> {
  let Some(raw) = raw else {
    return Ok(None);
  };
  let trimmed = raw.trim();
  if trimmed.is_empty() {
    return Ok(None);
  }
  let lower = trimmed.to_ascii_lowercase();
  if matches!(lower.as_str(), "1" | "true" | "yes" | "on") {
    return Ok(Some(true));
  }
  if matches!(lower.as_str(), "0" | "false" | "no" | "off") {
    return Ok(Some(false));
  }
  Err(SandboxEnvError::new(var, trimmed.to_string()))
}

/// Parse the debug escape hatch `FASTR_DISABLE_RENDERER_SANDBOX`.
///
/// Semantics match the docs:
/// - unset/empty => None
/// - `0|false|no|off` (case-insensitive) => Some(false)
/// - any other non-empty value => Some(true)
fn parse_optional_disable_flag(raw: Option<String>) -> Option<bool> {
  let Some(raw) = raw else {
    return None;
  };
  let trimmed = raw.trim();
  if trimmed.is_empty() {
    return None;
  }
  let lower = trimmed.to_ascii_lowercase();
  if matches!(lower.as_str(), "0" | "false" | "no" | "off") {
    return Some(false);
  }
  Some(true)
}

#[cfg(test)]
mod tests {
  use super::*;
  use std::collections::HashMap;

  #[test]
  fn parses_renderer_sandbox_config_from_env_map() {
    let defaults = RendererSandboxConfig {
      enabled: true,
      seccomp: true,
      landlock: true,
      close_fds: true,
    };

    let env = HashMap::<&str, &str>::new();
    assert_eq!(
      RendererSandboxConfig::from_lookup_with_defaults(
        |name| env.get(name).map(|v| (*v).to_string()),
        defaults
      ),
      Ok(defaults)
    );

    // Master switch disables everything.
    let env = HashMap::from([(ENV_DISABLE_RENDERER_SANDBOX, "1")]);
    assert_eq!(
      RendererSandboxConfig::from_lookup_with_defaults(
        |name| env.get(name).map(|v| (*v).to_string()),
        defaults
      ),
      Ok(RendererSandboxConfig {
        enabled: false,
        seccomp: false,
        landlock: false,
        close_fds: false,
      })
    );

    // Per-layer toggles apply when enabled.
    let env = HashMap::from([
      (ENV_DISABLE_RENDERER_SANDBOX, "0"),
      (ENV_RENDERER_SECCOMP, "0"),
      (ENV_RENDERER_CLOSE_FDS, "false"),
    ]);
    assert_eq!(
      RendererSandboxConfig::from_lookup_with_defaults(
        |name| env.get(name).map(|v| (*v).to_string()),
        defaults
      ),
      Ok(RendererSandboxConfig {
        enabled: true,
        seccomp: false,
        landlock: true,
        close_fds: false,
      })
    );
  }

  #[test]
  fn rejects_invalid_env_values() {
    let defaults = RendererSandboxConfig {
      enabled: true,
      seccomp: true,
      landlock: true,
      close_fds: true,
    };

    let env = HashMap::from([(ENV_RENDERER_SECCOMP, "maybe")]);
    let err = RendererSandboxConfig::from_lookup_with_defaults(
      |name| env.get(name).map(|v| (*v).to_string()),
      defaults,
    )
    .expect_err("expected invalid value to error");
    assert_eq!(err.var(), ENV_RENDERER_SECCOMP);
    assert_eq!(err.value(), "maybe");
  }
}
