//! Native `NativeRunner2` implementation backed by the `native-js` toolchain.
//!
//! This module is feature-gated because `native-js` pulls in LLVM-heavy dependencies.

use std::io::Read;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use diagnostics::{Diagnostic, FileId, Span, TextRange};
use native_js::compiler::compile_typescript_to_artifact;
use native_js::toolchain::LlvmToolchain;
use native_js::{compile_typescript_to_llvm_ir, CompileOptions, EmitKind};

use crate::{NativeRunner2, RunOutcome};

#[derive(Debug, Clone)]
pub struct NativeJsRunnerOptions {
  /// Timeout for the final native executable.
  pub run_timeout: Duration,
  /// Timeout for invoking `clang` when using the legacy IR→clang fallback.
  pub clang_timeout: Duration,
  /// Prefer `native_js::compiler::compile_typescript_to_artifact(..., EmitKind::Executable, ...)`
  /// when available. When this fails at runtime (missing runtime-native, missing lld, etc), the
  /// runner falls back to the older IR→clang pipeline.
  pub prefer_native_js_artifact_pipeline: bool,
}

impl Default for NativeJsRunnerOptions {
  fn default() -> Self {
    Self {
      run_timeout: Duration::from_secs(2),
      clang_timeout: Duration::from_secs(30),
      prefer_native_js_artifact_pipeline: true,
    }
  }
}

#[derive(Debug, Clone)]
pub struct NativeJsRunner {
  pub toolchain: LlvmToolchain,
  pub options: NativeJsRunnerOptions,
}

impl NativeJsRunner {
  pub fn new(toolchain: LlvmToolchain) -> Self {
    Self {
      toolchain,
      options: NativeJsRunnerOptions::default(),
    }
  }

  pub fn with_options(toolchain: LlvmToolchain, options: NativeJsRunnerOptions) -> Self {
    Self { toolchain, options }
  }
}

impl NativeRunner2 for NativeJsRunner {
  fn compile_and_run(&self, ts: &str) -> RunOutcome {
    if !cfg!(target_os = "linux") {
      return RunOutcome::CompileError {
        diagnostic: native_error("native-js runner is only supported on Linux"),
      };
    }

    if self.options.prefer_native_js_artifact_pipeline {
      match compile_and_run_via_native_js_artifact(ts, self.options.run_timeout) {
        Ok(outcome) => return outcome,
        Err(err) => {
          // Fall back to the IR→clang pipeline. This is intentionally best-effort: the artifact
          // pipeline requires runtime-native + lld/objcopy, while the old pipeline can run with
          // just `clang`.
          match compile_and_run_via_ir_and_clang(
            &self.toolchain,
            ts,
            self.options.clang_timeout,
            self.options.run_timeout,
          ) {
            Ok(outcome) => return outcome,
            Err(fallback_err) => {
              return RunOutcome::CompileError {
                diagnostic: native_error(format!(
                  "native-js compilation failed (artifact pipeline): {err}\n\
and fallback IR→clang compilation also failed: {fallback_err}"
                )),
              };
            }
          }
        }
      }
    }

    match compile_and_run_via_ir_and_clang(
      &self.toolchain,
      ts,
      self.options.clang_timeout,
      self.options.run_timeout,
    ) {
      Ok(outcome) => outcome,
      Err(err) => RunOutcome::CompileError {
        diagnostic: native_error(err),
      },
    }
  }
}

fn empty_span() -> Span {
  Span::new(FileId(0), TextRange::new(0, 0))
}

fn native_error(message: impl Into<String>) -> Diagnostic {
  Diagnostic::error("NATIVE0001", message, empty_span())
}

fn strip_one_trailing_newline(s: &mut String) {
  if s.ends_with('\n') {
    s.pop();
    if s.ends_with('\r') {
      s.pop();
    }
  }
}

fn bytes_to_captured_string(bytes: &[u8]) -> String {
  let mut s = String::from_utf8_lossy(bytes).into_owned();
  strip_one_trailing_newline(&mut s);
  s
}

fn run_with_timeout(mut cmd: Command, timeout: Duration) -> std::io::Result<(Output, bool)> {
  let mut child = cmd
    .stdin(Stdio::null())
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())
    .spawn()?;

  let mut stdout = Vec::new();
  let mut stderr = Vec::new();
  let mut out_reader = child.stdout.take().expect("stdout piped");
  let mut err_reader = child.stderr.take().expect("stderr piped");

  let start = Instant::now();
  let mut timed_out = false;
  let status = loop {
    if let Some(status) = child.try_wait()? {
      break status;
    }
    if start.elapsed() >= timeout {
      timed_out = true;
      let _ = child.kill();
      break child.wait()?;
    }
    std::thread::sleep(Duration::from_millis(5));
  };

  out_reader.read_to_end(&mut stdout)?;
  err_reader.read_to_end(&mut stderr)?;

  Ok((
    Output {
      status,
      stdout,
      stderr,
    },
    timed_out,
  ))
}

fn compile_and_run_via_native_js_artifact(
  ts: &str,
  run_timeout: Duration,
) -> Result<RunOutcome, String> {
  let mut opts = CompileOptions::default();
  opts.builtins = true;
  opts.emit = EmitKind::Executable;
  opts.debug = false;

  let artifact = compile_typescript_to_artifact(ts, opts, None).map_err(|err| err.to_string())?;

  let (out, timed_out) = run_with_timeout(Command::new(&artifact.path), run_timeout)
    .map_err(|err| format!("failed to run {}: {err}", artifact.path.display()))?;

  let stdout = bytes_to_captured_string(&out.stdout);
  let stderr = bytes_to_captured_string(&out.stderr);

  if timed_out {
    return Ok(RunOutcome::Terminated {
      message: format!("native executable timed out after {run_timeout:?}"),
      stdout,
      stderr,
    });
  }

  if !out.status.success() {
    return Ok(RunOutcome::Terminated {
      message: format!("native executable exited with status {}", out.status),
      stdout,
      stderr,
    });
  }

  Ok(RunOutcome::Ok {
    // The native-js AOT pipeline does not yet implement the oracle harness'
    // `globalThis.__native_result` observation protocol. Use a stable sentinel value so stdout-only
    // comparisons can still use `compare_run_outcomes`.
    value: "undefined".to_string(),
    stdout,
    stderr,
  })
}

struct TempDirGuard {
  path: PathBuf,
}

impl TempDirGuard {
  fn new(prefix: &str) -> std::io::Result<Self> {
    let base = std::env::temp_dir();
    let pid = std::process::id();
    let now = SystemTime::now()
      .duration_since(UNIX_EPOCH)
      .unwrap_or_default()
      .as_nanos();

    for attempt in 0..1000u32 {
      let path = base.join(format!("{prefix}{pid}-{now}-{attempt}"));
      match std::fs::create_dir(&path) {
        Ok(()) => return Ok(Self { path }),
        Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => continue,
        Err(err) => return Err(err),
      }
    }

    Err(std::io::Error::new(
      std::io::ErrorKind::AlreadyExists,
      "failed to create a unique temp directory",
    ))
  }
}

impl Drop for TempDirGuard {
  fn drop(&mut self) {
    let _ = std::fs::remove_dir_all(&self.path);
  }
}

fn compile_and_run_via_ir_and_clang(
  toolchain: &LlvmToolchain,
  ts: &str,
  clang_timeout: Duration,
  run_timeout: Duration,
) -> Result<RunOutcome, String> {
  let mut opts = CompileOptions::default();
  opts.builtins = true;

  let ir = compile_typescript_to_llvm_ir(ts, opts).map_err(|err| err.to_string())?;

  let td = TempDirGuard::new("native-oracle-harness-")
    .map_err(|err| format!("failed to create temp dir: {err}"))?;

  let ll_path = td.path.join("out.ll");
  std::fs::write(&ll_path, ir)
    .map_err(|err| format!("failed to write {}: {err}", ll_path.display()))?;

  let exe_path = td.path.join("out");
  let mut clang = Command::new(&toolchain.clang);
  clang
    .arg("-x")
    .arg("ir")
    .arg(&ll_path)
    .arg("-O0")
    .arg("-o")
    .arg(&exe_path);

  let (clang_out, clang_timed_out) = run_with_timeout(clang, clang_timeout)
    .map_err(|err| format!("failed to invoke clang: {err}"))?;

  if clang_timed_out {
    return Err(format!("clang timed out after {clang_timeout:?}"));
  }
  if !clang_out.status.success() {
    return Err(format!(
      "clang failed with status {status}\nstdout:\n{stdout}\nstderr:\n{stderr}",
      status = clang_out.status,
      stdout = String::from_utf8_lossy(&clang_out.stdout),
      stderr = String::from_utf8_lossy(&clang_out.stderr),
    ));
  }

  // Ensure the file is marked executable on Unix even if the toolchain emits it without +x.
  #[cfg(unix)]
  {
    use std::os::unix::fs::PermissionsExt;
    if let Ok(meta) = std::fs::metadata(&exe_path) {
      let mut perms = meta.permissions();
      perms.set_mode(perms.mode() | 0o111);
      let _ = std::fs::set_permissions(&exe_path, perms);
    }
  }

  let (out, timed_out) = run_with_timeout(Command::new(&exe_path), run_timeout)
    .map_err(|err| format!("failed to run {}: {err}", exe_path.display()))?;

  let stdout = bytes_to_captured_string(&out.stdout);
  let stderr = bytes_to_captured_string(&out.stderr);

  if timed_out {
    return Ok(RunOutcome::Terminated {
      message: format!("native executable timed out after {run_timeout:?}"),
      stdout,
      stderr,
    });
  }

  if !out.status.success() {
    return Ok(RunOutcome::Terminated {
      message: format!("native executable exited with status {}", out.status),
      stdout,
      stderr,
    });
  }

  Ok(RunOutcome::Ok {
    value: "undefined".to_string(),
    stdout,
    stderr,
  })
}
