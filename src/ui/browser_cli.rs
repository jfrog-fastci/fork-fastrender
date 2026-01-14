use std::fmt;

pub const ENV_WGPU_FALLBACK: &str = "FASTR_BROWSER_WGPU_FALLBACK";
pub const ENV_WGPU_BACKENDS: &str = "FASTR_BROWSER_WGPU_BACKENDS";
pub const ENV_DOWNLOAD_DIR: &str = crate::ui::downloads::ENV_BROWSER_DOWNLOAD_DIR;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BrowserWgpuOptions {
  pub backends: wgpu::Backends,
  pub force_fallback_adapter: bool,
}

impl Default for BrowserWgpuOptions {
  fn default() -> Self {
    Self {
      backends: wgpu::Backends::all(),
      force_fallback_adapter: false,
    }
  }
}

impl fmt::Display for BrowserWgpuOptions {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    write!(
      f,
      "backends={:?} force_fallback_adapter={}",
      self.backends, self.force_fallback_adapter
    )
  }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BrowserCliRunArgs {
  pub raw_url: Option<String>,
  pub wgpu_fallback: bool,
  pub wgpu_backends: Option<wgpu::Backends>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BrowserCliAction {
  Run(BrowserCliRunArgs),
  Help,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BrowserCliError {
  pub message: String,
}

impl BrowserCliError {
  pub fn new(message: impl Into<String>) -> Self {
    Self {
      message: message.into(),
    }
  }
}

impl fmt::Display for BrowserCliError {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    write!(f, "{}", self.message)
  }
}

impl std::error::Error for BrowserCliError {}

fn parse_env_bool(key: &str, raw: &str) -> Result<bool, BrowserCliError> {
  let v = raw.trim().to_ascii_lowercase();
  if v.is_empty() {
    return Ok(false);
  }

  if matches!(v.as_str(), "1" | "true" | "yes" | "on") {
    return Ok(true);
  }
  if matches!(v.as_str(), "0" | "false" | "no" | "off") {
    return Ok(false);
  }

  Err(BrowserCliError::new(format!(
    "{key}: invalid value {raw:?}; expected 0|1|true|false"
  )))
}

pub fn parse_wgpu_fallback_env(raw: Option<&str>) -> Result<bool, BrowserCliError> {
  let Some(raw) = raw else {
    return Ok(false);
  };
  parse_env_bool(ENV_WGPU_FALLBACK, raw)
}

pub fn parse_wgpu_backends(raw: &str) -> Result<wgpu::Backends, BrowserCliError> {
  let raw = raw.trim();
  if raw.is_empty() {
    return Err(BrowserCliError::new("wgpu backends cannot be empty"));
  }

  let raw = raw.to_ascii_lowercase();
  if matches!(raw.as_str(), "all" | "auto" | "default") {
    return Ok(wgpu::Backends::all());
  }

  let mut backends = wgpu::Backends::empty();
  for token in raw.split(',') {
    let token = token.trim();
    if token.is_empty() {
      continue;
    }

    backends |= match token {
      "vulkan" => wgpu::Backends::VULKAN,
      "metal" => wgpu::Backends::METAL,
      "dx12" | "d3d12" => wgpu::Backends::DX12,
      "dx11" | "d3d11" => wgpu::Backends::DX11,
      "gl" | "opengl" => wgpu::Backends::GL,
      // Useful for WASM builds; harmless on native.
      "webgpu" | "browser-webgpu" => wgpu::Backends::BROWSER_WEBGPU,
      other => {
        return Err(BrowserCliError::new(format!(
          "invalid wgpu backend {other:?}; expected vulkan|metal|dx12|dx11|gl|all"
        )));
      }
    };
  }

  if backends.is_empty() {
    return Err(BrowserCliError::new(
      "wgpu backends cannot be empty (parsed no backends)",
    ));
  }

  Ok(backends)
}

pub fn parse_wgpu_backends_env(
  raw: Option<&str>,
) -> Result<Option<wgpu::Backends>, BrowserCliError> {
  let Some(raw) = raw else {
    return Ok(None);
  };
  let raw = raw.trim();
  if raw.is_empty() {
    return Ok(None);
  }
  let parsed = parse_wgpu_backends(raw)
    .map_err(|err| BrowserCliError::new(format!("{ENV_WGPU_BACKENDS}: {}", err.message)))?;
  Ok(Some(parsed))
}

pub fn resolve_wgpu_options(
  cli_fallback: bool,
  cli_backends: Option<wgpu::Backends>,
  env_fallback: Option<&str>,
  env_backends: Option<&str>,
) -> Result<BrowserWgpuOptions, BrowserCliError> {
  let backends = match cli_backends {
    Some(backends) => backends,
    None => parse_wgpu_backends_env(env_backends)?.unwrap_or(wgpu::Backends::all()),
  };

  let force_fallback_adapter = if cli_fallback {
    true
  } else {
    parse_wgpu_fallback_env(env_fallback)?
  };

  Ok(BrowserWgpuOptions {
    backends,
    force_fallback_adapter,
  })
}

pub fn parse_browser_cli_args(args: &[String]) -> Result<BrowserCliAction, BrowserCliError> {
  let mut raw_url: Option<String> = None;
  let mut wgpu_fallback = false;
  let mut wgpu_backends: Option<wgpu::Backends> = None;

  let mut iter = args.iter().peekable();
  while let Some(arg) = iter.next() {
    match arg.as_str() {
      "-h" | "--help" => return Ok(BrowserCliAction::Help),
      "--wgpu-fallback" => {
        wgpu_fallback = true;
      }
      "--wgpu-backend" | "--wgpu-backends" => {
        let Some(value) = iter.next() else {
          return Err(BrowserCliError::new(format!(
            "missing value for {arg} (expected vulkan|metal|dx12|dx11|gl|all)"
          )));
        };
        if wgpu_backends.is_some() {
          return Err(BrowserCliError::new(format!(
            "wgpu backend was already set; remove duplicate {arg}"
          )));
        }
        wgpu_backends = Some(
          parse_wgpu_backends(value)
            .map_err(|err| BrowserCliError::new(format!("{arg}: {}", err.message)))?,
        );
      }
      other => {
        if let Some(value) = other
          .strip_prefix("--wgpu-backend=")
          .or_else(|| other.strip_prefix("--wgpu-backends="))
        {
          if wgpu_backends.is_some() {
            return Err(BrowserCliError::new(
              "wgpu backend was already set; remove duplicate --wgpu-backend/--wgpu-backends",
            ));
          }
          wgpu_backends = Some(
            parse_wgpu_backends(value)
              .map_err(|err| BrowserCliError::new(format!("--wgpu-backend: {}", err.message)))?,
          );
          continue;
        }

        if other.starts_with('-') {
          return Err(BrowserCliError::new(format!(
            "unexpected argument: {other:?}"
          )));
        }
        if raw_url.is_none() {
          raw_url = Some(other.to_string());
        } else {
          return Err(BrowserCliError::new(format!(
            "unexpected argument: {other:?}"
          )));
        }
      }
    }
  }

  Ok(BrowserCliAction::Run(BrowserCliRunArgs {
    raw_url,
    wgpu_fallback,
    wgpu_backends,
  }))
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn parse_browser_cli_help() {
    let args = vec!["--help".to_string()];
    assert_eq!(parse_browser_cli_args(&args), Ok(BrowserCliAction::Help));
    let args = vec!["-h".to_string()];
    assert_eq!(parse_browser_cli_args(&args), Ok(BrowserCliAction::Help));
  }

  #[test]
  fn parse_browser_cli_wgpu_flags() {
    let args = vec!["--wgpu-fallback".to_string()];
    let BrowserCliAction::Run(run) = parse_browser_cli_args(&args).unwrap() else {
      panic!("expected Run");
    };
    assert!(run.wgpu_fallback);
    assert_eq!(run.wgpu_backends, None);
    assert_eq!(run.raw_url, None);

    let args = vec!["--wgpu-backend".to_string(), "gl".to_string()];
    let BrowserCliAction::Run(run) = parse_browser_cli_args(&args).unwrap() else {
      panic!("expected Run");
    };
    assert!(!run.wgpu_fallback);
    assert_eq!(run.wgpu_backends, Some(wgpu::Backends::GL));
  }

  #[test]
  fn parse_browser_cli_positional_url() {
    let args = vec!["https://example.com/".to_string()];
    let BrowserCliAction::Run(run) = parse_browser_cli_args(&args).unwrap() else {
      panic!("expected Run");
    };
    assert_eq!(run.raw_url, Some("https://example.com/".to_string()));
  }

  #[test]
  fn parse_wgpu_fallback_env_values() {
    assert_eq!(parse_wgpu_fallback_env(None), Ok(false));
    assert_eq!(parse_wgpu_fallback_env(Some("")), Ok(false));
    assert_eq!(parse_wgpu_fallback_env(Some("0")), Ok(false));
    assert_eq!(parse_wgpu_fallback_env(Some("1")), Ok(true));
    assert_eq!(parse_wgpu_fallback_env(Some("true")), Ok(true));
    assert_eq!(parse_wgpu_fallback_env(Some("yes")), Ok(true));
    assert_eq!(parse_wgpu_fallback_env(Some("on")), Ok(true));
    assert_eq!(parse_wgpu_fallback_env(Some("no")), Ok(false));
    assert_eq!(parse_wgpu_fallback_env(Some("off")), Ok(false));
    assert!(parse_wgpu_fallback_env(Some("maybe")).is_err());
  }

  #[test]
  fn parse_wgpu_backends_values() {
    assert_eq!(parse_wgpu_backends("gl"), Ok(wgpu::Backends::GL));
    assert_eq!(parse_wgpu_backends("all"), Ok(wgpu::Backends::all()));
    assert_eq!(parse_wgpu_backends("auto"), Ok(wgpu::Backends::all()));
    assert_eq!(parse_wgpu_backends("default"), Ok(wgpu::Backends::all()));
    assert_eq!(
      parse_wgpu_backends("vulkan,gl"),
      Ok(wgpu::Backends::VULKAN | wgpu::Backends::GL)
    );
    assert!(parse_wgpu_backends("").is_err());
    assert!(parse_wgpu_backends("wat").is_err());
  }

  #[test]
  fn parse_wgpu_backends_env_values() {
    assert_eq!(parse_wgpu_backends_env(None), Ok(None));
    assert_eq!(parse_wgpu_backends_env(Some("")), Ok(None));
    assert_eq!(
      parse_wgpu_backends_env(Some("gl")),
      Ok(Some(wgpu::Backends::GL))
    );

    let err = parse_wgpu_backends_env(Some("wat")).expect_err("expected error");
    assert!(
      err.message.contains(ENV_WGPU_BACKENDS),
      "expected error to mention env var name, got: {}",
      err.message
    );
  }

  #[test]
  fn resolve_wgpu_options_prefers_cli_over_env() {
    let options =
      resolve_wgpu_options(true, Some(wgpu::Backends::GL), Some("0"), Some("vulkan")).unwrap();
    assert!(options.force_fallback_adapter);
    assert_eq!(options.backends, wgpu::Backends::GL);
  }

  #[test]
  fn resolve_wgpu_options_uses_env_when_cli_is_unset() {
    let options = resolve_wgpu_options(false, None, Some("1"), Some("gl")).unwrap();
    assert!(options.force_fallback_adapter);
    assert_eq!(options.backends, wgpu::Backends::GL);
  }
}
