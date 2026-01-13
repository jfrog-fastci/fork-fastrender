//! Linux sandbox probe tool.
//!
//! This binary is intended for iterating on the renderer sandbox policy (seccomp/landlock) without
//! needing to run the full multi-process browser stack.

#[cfg(not(target_os = "linux"))]
fn main() {
  eprintln!("sandbox_probe is only supported on Linux.");
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

  pub(crate) fn run() -> i32 {
    let args = Args::parse();
    println!("mode: {:?}", args.mode);
    println!("probe: {:?}", args.probe);

    if let Err(err) = apply_sandbox_layers(args.mode) {
      eprintln!("sandbox: failed to apply: {err}");
      // Distinct from probe failures so scripts can detect "sandbox didn't load".
      return 2;
    }
    if !matches!(args.mode, SandboxMode::None) {
      println!("sandbox: applied");
    }

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

  fn apply_sandbox_layers(mode: SandboxMode) -> Result<(), String> {
    match mode {
      SandboxMode::None => Ok(()),
      SandboxMode::Seccomp => apply_seccomp_only(),
      SandboxMode::Landlock => apply_landlock_only(),
      SandboxMode::Full => {
        // Landlock is best-effort: older kernels may not support it. We still apply seccomp so the
        // probe remains useful for validating syscall filtering.
        apply_landlock_best_effort()?;
        apply_seccomp_only()
      }
    }
  }

  fn apply_seccomp_only() -> Result<(), String> {
    match sandbox::apply_renderer_seccomp_denylist() {
      Ok(sandbox::SandboxStatus::Applied) => Ok(()),
      Ok(sandbox::SandboxStatus::Unsupported) => Err("seccomp sandbox unsupported".to_string()),
      Err(err) => Err(err.to_string()),
    }
  }

  fn apply_landlock_only() -> Result<(), String> {
    match sandbox::linux_landlock::apply(&sandbox::linux_landlock::LandlockConfig::default()) {
      Ok(sandbox::linux_landlock::LandlockStatus::Applied { abi }) => {
        println!("landlock: applied (abi {abi})");
        Ok(())
      }
      Ok(sandbox::linux_landlock::LandlockStatus::Unsupported { reason }) => {
        Err(format!("landlock unsupported ({reason:?})"))
      }
      Err(err) => Err(err.to_string()),
    }
  }

  fn apply_landlock_best_effort() -> Result<(), String> {
    match sandbox::linux_landlock::apply(&sandbox::linux_landlock::LandlockConfig::default()) {
      Ok(sandbox::linux_landlock::LandlockStatus::Applied { abi }) => {
        println!("landlock: applied (abi {abi})");
        Ok(())
      }
      Ok(sandbox::linux_landlock::LandlockStatus::Unsupported { reason }) => {
        println!("landlock: unsupported ({reason:?})");
        Ok(())
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
