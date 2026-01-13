use crate::sandbox::SandboxError;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

pub const ENV_RENDERER_SANDBOX: &str = "FASTR_RENDERER_SANDBOX";
pub const ENV_MACOS_SEATBELT_PROFILE: &str = "FASTR_RENDERER_MACOS_SEATBELT_PROFILE";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MacosSeatbeltProfileSelection {
  PureComputation,
  NoInternet,
  RendererDefault,
  SbplPath { raw: String, path: PathBuf },
}

impl MacosSeatbeltProfileSelection {
  pub fn describe(&self) -> String {
    match self {
      Self::PureComputation => "pure-computation".to_string(),
      Self::NoInternet => "no-internet".to_string(),
      Self::RendererDefault => "renderer-default".to_string(),
      Self::SbplPath { raw, .. } => format!("sbpl-file:{raw}"),
    }
  }

  pub fn load_sbpl_source(&self) -> Result<String, SandboxError> {
    match self {
      Self::SbplPath { raw, path } => {
        let sbpl = std::fs::read_to_string(path).map_err(|source| {
          SandboxError::ReadSeatbeltProfileFailed {
            var: ENV_MACOS_SEATBELT_PROFILE,
            raw_value: raw.clone(),
            path: path.clone(),
            source,
          }
        })?;
        if sbpl.as_bytes().contains(&0) {
          return Err(SandboxError::SeatbeltProfileContainsNul {
            var: ENV_MACOS_SEATBELT_PROFILE,
            raw_value: raw.clone(),
          });
        }
        Ok(sbpl)
      }
      _ => Err(SandboxError::InvalidMacosSeatbeltProfile {
        var: ENV_MACOS_SEATBELT_PROFILE,
        value: self.describe(),
      }),
    }
  }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RendererSandboxEnvConfig {
  pub enabled: bool,
  pub macos_seatbelt_profile: MacosSeatbeltProfileSelection,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RendererSandboxMode {
  Strict,
  Relaxed,
  Off,
}

impl RendererSandboxEnvConfig {
  pub fn from_env(default_enabled: bool) -> Result<Self, SandboxError> {
    Self::from_vars_iter(std::env::vars(), default_enabled)
  }

  pub(crate) fn from_vars_iter(
    vars: impl IntoIterator<Item = (String, String)>,
    default_enabled: bool,
  ) -> Result<Self, SandboxError> {
    let raw = vars.into_iter().collect::<HashMap<_, _>>();
    Self::from_env_map(&raw, default_enabled)
  }

  pub fn from_env_map(
    env: &HashMap<String, String>,
    default_enabled: bool,
  ) -> Result<Self, SandboxError> {
    let mode = parse_renderer_sandbox_mode(
      env.get(ENV_RENDERER_SANDBOX).map(String::as_str),
      default_enabled,
    )?;
    let enabled = mode != RendererSandboxMode::Off;

    let default_profile = match mode {
      RendererSandboxMode::Strict => MacosSeatbeltProfileSelection::PureComputation,
      RendererSandboxMode::Relaxed => MacosSeatbeltProfileSelection::RendererDefault,
      RendererSandboxMode::Off => MacosSeatbeltProfileSelection::PureComputation,
    };
    let macos_seatbelt_profile = parse_macos_seatbelt_profile(
      env.get(ENV_MACOS_SEATBELT_PROFILE).map(String::as_str),
      default_profile,
    )?;
    Ok(Self {
      enabled,
      macos_seatbelt_profile,
    })
  }
}

fn parse_renderer_sandbox_mode(
  value: Option<&str>,
  default_enabled: bool,
) -> Result<RendererSandboxMode, SandboxError> {
  let Some(raw) = value else {
    return Ok(if default_enabled {
      RendererSandboxMode::Strict
    } else {
      RendererSandboxMode::Off
    });
  };
  let trimmed = raw.trim();

  if trimmed.is_empty() {
    return Err(SandboxError::InvalidBoolean0Or1 {
      var: ENV_RENDERER_SANDBOX,
      value: raw.to_string(),
    });
  }

  // Accept canonical and case-insensitive spellings.
  //
  // `0|1` are legacy spellings. `strict|relaxed|off` are the preferred values for renderer
  // sandbox mode selection.
  let lower = trimmed.to_ascii_lowercase();
  match lower.as_str() {
    "0" | "off" => Ok(RendererSandboxMode::Off),
    "1" | "strict" => Ok(RendererSandboxMode::Strict),
    "relaxed" => Ok(RendererSandboxMode::Relaxed),
    _ => Err(SandboxError::InvalidBoolean0Or1 {
      var: ENV_RENDERER_SANDBOX,
      value: raw.to_string(),
    }),
  }
}

fn parse_macos_seatbelt_profile(
  value: Option<&str>,
  default: MacosSeatbeltProfileSelection,
) -> Result<MacosSeatbeltProfileSelection, SandboxError> {
  let Some(raw) = value else {
    return Ok(default);
  };
  let trimmed = raw.trim();
  if trimmed.is_empty() {
    return Err(SandboxError::InvalidMacosSeatbeltProfile {
      var: ENV_MACOS_SEATBELT_PROFILE,
      value: raw.to_string(),
    });
  }

  // Accept both canonical and case-insensitive spellings.
  let lower = trimmed.to_ascii_lowercase();
  match lower.as_str() {
    "pure-computation" => Ok(MacosSeatbeltProfileSelection::PureComputation),
    "no-internet" => Ok(MacosSeatbeltProfileSelection::NoInternet),
    "renderer-default" => Ok(MacosSeatbeltProfileSelection::RendererDefault),
    _ => Ok(MacosSeatbeltProfileSelection::SbplPath {
      raw: trimmed.to_string(),
      path: Path::new(trimmed).to_path_buf(),
    }),
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn renderer_sandbox_enabled_defaults() {
    let env = HashMap::new();
    let config = RendererSandboxEnvConfig::from_env_map(&env, true).expect("parse config");
    assert!(config.enabled);
    assert_eq!(
      config.macos_seatbelt_profile,
      MacosSeatbeltProfileSelection::PureComputation
    );
    let config = RendererSandboxEnvConfig::from_env_map(&env, false).expect("parse config");
    assert!(!config.enabled);
  }

  #[test]
  fn renderer_sandbox_enabled_parses_0_1_with_whitespace() {
    let mut env = HashMap::new();
    env.insert(ENV_RENDERER_SANDBOX.to_string(), " 0 ".to_string());
    let config = RendererSandboxEnvConfig::from_env_map(&env, true).expect("parse config");
    assert!(!config.enabled);
    env.insert(ENV_RENDERER_SANDBOX.to_string(), "\t1\n".to_string());
    let config = RendererSandboxEnvConfig::from_env_map(&env, false).expect("parse config");
    assert!(config.enabled);
    assert_eq!(
      config.macos_seatbelt_profile,
      MacosSeatbeltProfileSelection::PureComputation
    );
  }

  #[test]
  fn renderer_sandbox_enabled_rejects_unknown_values() {
    let mut env = HashMap::new();
    env.insert(ENV_RENDERER_SANDBOX.to_string(), "2".to_string());
    let err = RendererSandboxEnvConfig::from_env_map(&env, true).unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("FASTR_RENDERER_SANDBOX"), "msg={msg}");
    assert!(msg.contains("strict"), "msg={msg}");
  }

  #[test]
  fn macos_seatbelt_profile_defaults_to_pure_computation() {
    let env = HashMap::new();
    let config = RendererSandboxEnvConfig::from_env_map(&env, true).expect("parse config");
    assert_eq!(
      config.macos_seatbelt_profile,
      MacosSeatbeltProfileSelection::PureComputation
    );
  }

  #[test]
  fn macos_seatbelt_profile_parses_named_profiles_case_insensitive() {
    let mut env = HashMap::new();
    env.insert(
      ENV_MACOS_SEATBELT_PROFILE.to_string(),
      " Pure-Computation ".to_string(),
    );
    let config = RendererSandboxEnvConfig::from_env_map(&env, true).expect("parse config");
    assert_eq!(config.macos_seatbelt_profile, MacosSeatbeltProfileSelection::PureComputation);
  }

  #[test]
  fn macos_seatbelt_profile_treats_unknown_as_path() {
    let mut env = HashMap::new();
    env.insert(
      ENV_MACOS_SEATBELT_PROFILE.to_string(),
      "/tmp/custom.sbpl".to_string(),
    );
    let config = RendererSandboxEnvConfig::from_env_map(&env, true).expect("parse config");
    assert_eq!(
      config.macos_seatbelt_profile,
      MacosSeatbeltProfileSelection::SbplPath {
        raw: "/tmp/custom.sbpl".to_string(),
        path: PathBuf::from("/tmp/custom.sbpl")
      }
    );
  }

  #[test]
  fn macos_seatbelt_profile_rejects_empty_string() {
    let mut env = HashMap::new();
    env.insert(ENV_MACOS_SEATBELT_PROFILE.to_string(), "   ".to_string());
    let err = RendererSandboxEnvConfig::from_env_map(&env, true).unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("FASTR_RENDERER_MACOS_SEATBELT_PROFILE"), "msg={msg}");
    assert!(msg.contains("pure-computation"), "msg={msg}");
  }

  #[test]
  fn renderer_sandbox_mode_parses_strict_relaxed_off() {
    let mut env = HashMap::new();
    env.insert(ENV_RENDERER_SANDBOX.to_string(), "strict".to_string());
    let config = RendererSandboxEnvConfig::from_env_map(&env, false).expect("parse config");
    assert!(config.enabled);
    assert_eq!(
      config.macos_seatbelt_profile,
      MacosSeatbeltProfileSelection::PureComputation
    );

    env.insert(ENV_RENDERER_SANDBOX.to_string(), " relaxed ".to_string());
    let config = RendererSandboxEnvConfig::from_env_map(&env, false).expect("parse config");
    assert!(config.enabled);
    assert_eq!(
      config.macos_seatbelt_profile,
      MacosSeatbeltProfileSelection::RendererDefault
    );

    env.insert(ENV_RENDERER_SANDBOX.to_string(), "OFF".to_string());
    let config = RendererSandboxEnvConfig::from_env_map(&env, true).expect("parse config");
    assert!(!config.enabled);
  }
}
