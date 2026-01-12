#![cfg(feature = "native-js-runner")]

use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::Duration;

use native_js::toolchain::LlvmToolchain;
use native_js::{compile_typescript_to_llvm_ir, CompileOptions};
use native_oracle_harness::{
  compare_run_outcomes, run_fixture_ts_outcome_with_name, RunOutcome, RunOutcomeCompareOptions,
};
use wait_timeout::ChildExt;

fn run_with_timeout(mut cmd: Command, timeout: Duration) -> std::io::Result<Output> {
  let mut child = cmd.stdout(Stdio::piped()).stderr(Stdio::piped()).spawn()?;
  let mut stdout = Vec::new();
  let mut stderr = Vec::new();
  let mut out_reader = child.stdout.take().expect("stdout piped");
  let mut err_reader = child.stderr.take().expect("stderr piped");

  let status = match child.wait_timeout(timeout)? {
    Some(status) => status,
    None => {
      let _ = child.kill();
      child.wait()?
    }
  };

  out_reader.read_to_end(&mut stdout)?;
  err_reader.read_to_end(&mut stderr)?;

  Ok(Output {
    status,
    stdout,
    stderr,
  })
}

fn run_native_outcome(tc: &LlvmToolchain, ts_source: &str) -> Result<RunOutcome, String> {
  let mut opts = CompileOptions::default();
  opts.builtins = true;
  let ir = compile_typescript_to_llvm_ir(ts_source, opts).map_err(|err| err.to_string())?;

  let td = tempfile::tempdir().map_err(|err| format!("failed to create tempdir: {err}"))?;
  let ll_path = td.path().join("out.ll");
  std::fs::write(&ll_path, ir)
    .map_err(|err| format!("failed to write {}: {err}", ll_path.display()))?;

  let exe_path = td.path().join("out");
  let mut cmd = Command::new(&tc.clang);
  cmd
    .arg("-x")
    .arg("ir")
    .arg(&ll_path)
    .arg("-O0")
    .arg("-o")
    .arg(&exe_path);
  let out = run_with_timeout(cmd, Duration::from_secs(30))
    .map_err(|err| format!("failed to invoke clang: {err}"))?;

  if !out.status.success() {
    return Err(format!(
      "clang failed with status {status}\nstdout:\n{stdout}\nstderr:\n{stderr}",
      status = out.status,
      stdout = String::from_utf8_lossy(&out.stdout),
      stderr = String::from_utf8_lossy(&out.stderr)
    ));
  }

  let out = run_with_timeout(Command::new(&exe_path), Duration::from_secs(2))
    .map_err(|err| format!("failed to run {}: {err}", exe_path.display()))?;
  if !out.status.success() {
    return Err(format!(
      "native program exited with status {status}\nstdout:\n{stdout}\nstderr:\n{stderr}",
      status = out.status,
      stdout = String::from_utf8_lossy(&out.stdout),
      stderr = String::from_utf8_lossy(&out.stderr)
    ));
  }

  let stdout = String::from_utf8_lossy(&out.stdout).trim_end().to_string();
  let stderr = String::from_utf8_lossy(&out.stderr).trim_end().to_string();
  Ok(RunOutcome::Ok {
    // The `native_compare` fixture corpus only compares stdout. The oracle side uses the
    // `globalThis.__native_result` observation protocol, which yields `"undefined"` for these
    // fixtures. Keep the value stable so RunOutcome comparisons can focus on stdout.
    value: "undefined".to_string(),
    stdout,
    stderr,
  })
}

fn fixtures_dir() -> PathBuf {
  let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
  manifest_dir
    .parent()
    .expect("native-oracle-harness should live under vendor/ecma-rs/")
    .join("fixtures/native_compare")
}

#[test]
fn native_compare_fixtures_stdout_matches_oracle() {
  if !cfg!(target_os = "linux") {
    eprintln!("skipping native-compare fixtures: native-js runner is only supported on Linux");
    return;
  }

  let tc = match LlvmToolchain::detect() {
    Ok(tc) => tc,
    Err(err) => {
      eprintln!("skipping native-compare fixtures: {err}");
      return;
    }
  };

  let dir = fixtures_dir();
  let mut fixtures: Vec<PathBuf> = fs::read_dir(&dir)
    .unwrap_or_else(|err| panic!("failed to read fixture dir {}: {err}", dir.display()))
    .filter_map(|entry| entry.ok().map(|entry| entry.path()))
    .filter(|path| matches!(path.extension().and_then(|e| e.to_str()), Some("ts") | Some("tsx")))
    .collect();
  fixtures.sort();

  assert!(
    !fixtures.is_empty(),
    "expected at least one fixture under {}",
    dir.display()
  );

  for path in fixtures {
    let name = path
      .file_name()
      .and_then(|s| s.to_str())
      .unwrap_or("<fixture>");
    let ts =
      fs::read_to_string(&path).unwrap_or_else(|err| panic!("failed to read fixture {name}: {err}"));

    let mut oracle = run_fixture_ts_outcome_with_name(name, &ts);
    // Normalize stdout for stable comparisons (native output includes a trailing newline).
    if let RunOutcome::Ok { stdout, .. } = &mut oracle {
      *stdout = stdout.trim_end().to_string();
    }
    let native = run_native_outcome(&tc, &ts).unwrap_or_else(|err| panic!("native failed for {name}: {err}"));

    let opts = RunOutcomeCompareOptions {
      compare_stdout: true,
      ..RunOutcomeCompareOptions::default()
    };
    if let Err(err) = compare_run_outcomes(&oracle, &native, opts) {
      panic!(
        "native/vm-js mismatch for fixture `{name}`: {err}\n\
\n\
oracle outcome: {oracle:?}\n\
native outcome: {native:?}\n\
\n\
TypeScript source:\n{ts}\n"
      );
    }
  }
}
