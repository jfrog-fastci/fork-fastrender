#[cfg(not(target_os = "linux"))]
fn main() {
  eprintln!("sandbox_probe is currently Linux-only.");
  eprintln!("On macOS, see docs/security/macos_renderer_sandbox.md.");
  eprintln!("On Windows, see docs/security/windows_renderer_sandbox.md.");
  std::process::exit(2);
}

#[cfg(target_os = "linux")]
fn main() {
  std::process::exit(enabled::run());
}

#[cfg(target_os = "linux")]
mod enabled {
  use clap::{Parser, ValueEnum};
  use std::fs;
  use std::io;
  use std::net::TcpListener;
  use std::process::Command;

  use fastrender::sandbox;
  use fastrender::system::renderer_sandbox::RendererSandboxConfig as EnvSandboxConfig;

  #[derive(Parser)]
  #[command(about = "Probe FastRender's Linux renderer sandbox (seccomp/landlock) behavior")]
  struct Args {
    /// Which sandbox layers to apply.
    #[arg(
      long,
      value_enum,
      env = "FASTRENDER_SANDBOX_MODE",
      default_value = "full"
    )]
    mode: SandboxMode,

    /// Which probes to run.
    #[arg(
      long,
      value_enum,
      env = "FASTRENDER_SANDBOX_PROBE",
      default_value = "all"
    )]
    probe: ProbeKind,
  }

  #[derive(Clone, Copy, Debug, ValueEnum)]
  enum SandboxMode {
    None,
    Seccomp,
    Landlock,
    Full,
  }

  #[derive(Clone, Copy, Debug, ValueEnum)]
  enum ProbeKind {
    Fs,
    Net,
    All,
  }

  #[derive(Debug, Clone, Copy)]
  struct Expectations {
    fs_allowed: bool,
    net_allowed: bool,
    exec_allowed: bool,
  }

  fn expectations_for(mode: SandboxMode) -> Expectations {
    match mode {
      SandboxMode::None => Expectations {
        fs_allowed: true,
        net_allowed: true,
        exec_allowed: true,
      },
      SandboxMode::Seccomp => Expectations {
        fs_allowed: false,
        net_allowed: false,
        exec_allowed: false,
      },
      // Landlock denies filesystem access (including executing new binaries), but does not mediate
      // networking.
      SandboxMode::Landlock => Expectations {
        fs_allowed: false,
        net_allowed: true,
        exec_allowed: false,
      },
      SandboxMode::Full => Expectations {
        fs_allowed: false,
        net_allowed: false,
        exec_allowed: false,
      },
    }
  }

  #[derive(Debug)]
  enum ActionResult {
    Success(String),
    Failure(io::Error),
  }

  impl ActionResult {
    fn success(msg: impl Into<String>) -> Self {
      Self::Success(msg.into())
    }

    fn failure(err: io::Error) -> Self {
      Self::Failure(err)
    }
  }

  #[derive(Debug, Clone)]
  enum LayerOutcome {
    Applied,
    Skipped(&'static str),
    Unsupported(&'static str),
  }

  impl LayerOutcome {
    fn as_str(&self) -> &'static str {
      match self {
        Self::Applied => "applied",
        Self::Skipped(_) => "skipped",
        Self::Unsupported(_) => "unsupported",
      }
    }

    fn reason(&self) -> Option<&str> {
      match self {
        Self::Skipped(reason) | Self::Unsupported(reason) => Some(*reason),
        Self::Applied => None,
      }
    }
  }

  #[derive(Debug, Clone)]
  struct LayerReport {
    close_fds: LayerOutcome,
    seccomp: LayerOutcome,
    landlock: LayerOutcome,
  }

  pub(crate) fn run() -> i32 {
    let args = Args::parse();
    println!("mode: {:?}", args.mode);
    println!("probe: {:?}", args.probe);

    // Treat `--mode` as the baseline configuration. Sandbox env vars can opt out of layers, but
    // cannot opt into a stricter mode than requested by `--mode`.
    let mode_defaults = defaults_for_mode(args.mode);
    let env_cfg = match EnvSandboxConfig::from_env_with_defaults(mode_defaults) {
      Ok(cfg) => cfg,
      Err(err) => {
        eprintln!("sandbox_probe: invalid sandbox env var: {err}");
        return 2;
      }
    };

    // Apply disable-only semantics for the layers controlled by `--mode`.
    let mut cfg = EnvSandboxConfig {
      enabled: mode_defaults.enabled && env_cfg.enabled,
      seccomp: mode_defaults.seccomp && env_cfg.seccomp,
      landlock: mode_defaults.landlock && env_cfg.landlock,
      close_fds: env_cfg.close_fds,
    };

    if !cfg.enabled {
      cfg.seccomp = false;
      cfg.landlock = false;
      cfg.close_fds = false;
    }

    println!(
      "config: enabled={} seccomp={} landlock={} close_fds={}",
      cfg.enabled, cfg.seccomp, cfg.landlock, cfg.close_fds
    );

    let report = match apply_sandbox_layers(args.mode, cfg) {
      Ok(report) => report,
      Err(err) => {
        eprintln!("sandbox: failed to apply: {err}");
        // Distinct from probe failures so scripts can detect "sandbox didn't load".
        return 2;
      }
    };

    println!("layers:");
    println!(
      "  close_fds: {}{}",
      report.close_fds.as_str(),
      format_reason(report.close_fds.reason())
    );
    println!(
      "  landlock:  {}{}",
      report.landlock.as_str(),
      format_reason(report.landlock.reason())
    );
    println!(
      "  seccomp:   {}{}",
      report.seccomp.as_str(),
      format_reason(report.seccomp.reason())
    );

    let expectations = expectations_for(args.mode);
    let mut unexpected = false;

    if matches!(args.probe, ProbeKind::Fs | ProbeKind::All) {
      let result = probe_read_passwd();
      unexpected |= report_action("read /etc/passwd", result, !expectations.fs_allowed);
    }

    if matches!(args.probe, ProbeKind::Net | ProbeKind::All) {
      let result = probe_bind_tcp();
      unexpected |= report_action(
        "bind tcp socket (127.0.0.1:0)",
        result,
        !expectations.net_allowed,
      );
    }

    if matches!(args.probe, ProbeKind::All) {
      let result = probe_exec_sh();
      unexpected |= report_action("exec sh", result, !expectations.exec_allowed);
    }

    let exit_code = if unexpected { 1 } else { 0 };
    println!("exit_code: {exit_code}");
    exit_code
  }

  fn format_reason(reason: Option<&str>) -> String {
    match reason {
      Some(reason) => format!(" ({reason})"),
      None => String::new(),
    }
  }

  fn defaults_for_mode(mode: SandboxMode) -> EnvSandboxConfig {
    match mode {
      SandboxMode::None => EnvSandboxConfig {
        enabled: false,
        seccomp: false,
        landlock: false,
        close_fds: false,
      },
      SandboxMode::Seccomp => EnvSandboxConfig {
        enabled: true,
        seccomp: true,
        landlock: false,
        close_fds: false,
      },
      SandboxMode::Landlock => EnvSandboxConfig {
        enabled: true,
        seccomp: false,
        landlock: true,
        close_fds: false,
      },
      SandboxMode::Full => EnvSandboxConfig {
        enabled: true,
        seccomp: true,
        landlock: true,
        close_fds: false,
      },
    }
  }

  fn apply_sandbox_layers(mode: SandboxMode, cfg: EnvSandboxConfig) -> Result<LayerReport, String> {
    if !cfg.enabled {
      return Ok(LayerReport {
        close_fds: LayerOutcome::Skipped("sandbox disabled"),
        seccomp: LayerOutcome::Skipped("sandbox disabled"),
        landlock: LayerOutcome::Skipped("sandbox disabled"),
      });
    }

    let close_fds = if cfg.close_fds {
      match sandbox::close_fds_except(&[0, 1, 2]) {
        Ok(()) => LayerOutcome::Applied,
        Err(err) if err.kind() == io::ErrorKind::Unsupported => {
          LayerOutcome::Unsupported("unsupported platform")
        }
        Err(err) => return Err(err.to_string()),
      }
    } else {
      LayerOutcome::Skipped("disabled by config")
    };

    let landlock = if cfg.landlock {
      match mode {
        SandboxMode::Full => apply_landlock_best_effort()?,
        SandboxMode::Landlock => apply_landlock_only()?,
        // Other modes don't request Landlock by default.
        SandboxMode::None | SandboxMode::Seccomp => LayerOutcome::Skipped("not requested by mode"),
      }
    } else {
      LayerOutcome::Skipped("disabled by config")
    };

    let seccomp = if cfg.seccomp {
      match sandbox::apply_renderer_seccomp_denylist() {
        Ok(sandbox::SandboxStatus::Applied) => LayerOutcome::Applied,
        Ok(sandbox::SandboxStatus::AppliedWithoutTsync) => {
          println!("seccomp: applied without TSYNC (must sandbox before threads spawn)");
          LayerOutcome::Applied
        }
        Ok(sandbox::SandboxStatus::Unsupported) => {
          return Err("seccomp sandbox unsupported".to_string());
        }
        Ok(
          sandbox::SandboxStatus::DisabledByEnv
          | sandbox::SandboxStatus::DisabledByConfig
          | sandbox::SandboxStatus::ReportOnly,
        ) => return Err("seccomp sandbox reported disabled".to_string()),
        Err(err) => return Err(err.to_string()),
      }
    } else {
      LayerOutcome::Skipped("disabled by config")
    };

    Ok(LayerReport {
      close_fds,
      landlock,
      seccomp,
    })
  }

  fn apply_landlock_only() -> Result<LayerOutcome, String> {
    match sandbox::linux_landlock::apply(&sandbox::linux_landlock::LandlockConfig::default()) {
      Ok(sandbox::linux_landlock::LandlockStatus::Applied { abi }) => {
        println!("landlock: applied (abi {abi})");
        Ok(LayerOutcome::Applied)
      }
      Ok(sandbox::linux_landlock::LandlockStatus::Unsupported { reason }) => {
        Err(format!("landlock unsupported ({reason:?})"))
      }
      Err(err) => Err(err.to_string()),
    }
  }

  fn apply_landlock_best_effort() -> Result<LayerOutcome, String> {
    match sandbox::linux_landlock::apply(&sandbox::linux_landlock::LandlockConfig::default()) {
      Ok(sandbox::linux_landlock::LandlockStatus::Applied { abi }) => {
        println!("landlock: applied (abi {abi})");
        Ok(LayerOutcome::Applied)
      }
      Ok(sandbox::linux_landlock::LandlockStatus::Unsupported { reason }) => {
        println!("landlock: unsupported ({reason:?})");
        Ok(LayerOutcome::Unsupported("unsupported"))
      }
      Err(err) => Err(err.to_string()),
    }
  }

  fn probe_read_passwd() -> ActionResult {
    match fs::read("/etc/passwd") {
      Ok(bytes) => ActionResult::success(format!("read {} bytes", bytes.len())),
      Err(err) => ActionResult::failure(err),
    }
  }

  fn probe_bind_tcp() -> ActionResult {
    match TcpListener::bind(("127.0.0.1", 0)) {
      Ok(listener) => {
        let addr = listener
          .local_addr()
          .map(|addr| addr.to_string())
          .unwrap_or_else(|_| "<unknown>".to_string());
        ActionResult::success(format!("bound to {addr}"))
      }
      Err(err) => ActionResult::failure(err),
    }
  }

  fn probe_exec_sh() -> ActionResult {
    match Command::new("sh").arg("-c").arg("exit 0").status() {
      Ok(status) => ActionResult::success(format!("exit={status}")),
      Err(err) => ActionResult::failure(err),
    }
  }

  fn report_action(label: &str, result: ActionResult, expect_blocked: bool) -> bool {
    match result {
      ActionResult::Success(msg) => {
        println!("{label}: ALLOWED ({msg})");
        if expect_blocked {
          eprintln!("{label}: expected BLOCKED, got ALLOWED");
          return true;
        }
        false
      }
      ActionResult::Failure(err) => {
        println!("{label}: BLOCKED ({err})");
        if !expect_blocked {
          eprintln!("{label}: expected ALLOWED, got BLOCKED ({err})");
          return true;
        }
        false
      }
    }
  }
}
