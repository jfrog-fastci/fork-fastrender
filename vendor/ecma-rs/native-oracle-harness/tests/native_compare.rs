#![cfg(feature = "native-js-runner")]

use std::fs;
use std::path::{Path, PathBuf};

use native_js::toolchain::LlvmToolchain;
use native_oracle_harness::expectations::{load_expectations, ExpectMode, FixtureExpectation};
use native_oracle_harness::native_js_runner::NativeJsRunner;
use native_oracle_harness::{
  compare_run_outcomes, run_fixture_ts_outcome_with_name, NativeRunner2, RunOutcome,
  RunOutcomeCompareOptions,
};

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
  let runner = NativeJsRunner::new(tc);

  let dir = fixtures_dir();
  let expectations_path = dir.join("expectations.toml");
  let expectations = load_expectations(&expectations_path);
  let default = expectations
    .get("default")
    .cloned()
    .unwrap_or_else(FixtureExpectation::pass);

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
    let file_name = path
      .file_name()
      .and_then(|s| s.to_str())
      .unwrap_or("<fixture>");
    let key = path.file_stem().and_then(|s| s.to_str()).unwrap_or(file_name);

    let exp = expectations.get(key).cloned().unwrap_or_else(|| default.clone());
    if exp.mode == ExpectMode::Skip {
      if let Some(reason) = exp.reason.as_deref() {
        println!("SKIP {key}: {reason}");
      } else {
        println!("SKIP {key}");
      }
      continue;
    }

    let ts =
      fs::read_to_string(&path).unwrap_or_else(|err| panic!("failed to read fixture {file_name}: {err}"));

    // Always run the oracle first; fixtures should be valid oracle-side programs even when native
    // execution is marked xfail.
    let oracle = run_fixture_ts_outcome_with_name(file_name, &ts);

    let native = NativeRunner2::compile_and_run(&runner, &ts);

    let opts = RunOutcomeCompareOptions {
      compare_stdout: true,
      ..RunOutcomeCompareOptions::default()
    };

    match exp.mode {
      ExpectMode::Pass => {
        if let Err(err) = compare_run_outcomes(&oracle, &native, opts) {
          panic!(
            "native/vm-js mismatch for fixture `{file_name}`: {err}\n\
\n\
oracle outcome: {oracle:?}\n\
native outcome: {native:?}\n\
\n\
TypeScript source:\n{ts}\n"
          );
        }
      }

      ExpectMode::XfailCompile => match native {
        RunOutcome::CompileError { diagnostic } => {
          if let Some(reason) = exp.reason.as_deref() {
            println!("XFAIL-COMPILE {key}: {reason}");
          } else {
            println!("XFAIL-COMPILE {key}");
          }
          println!("  native compile failed as expected: {}: {}", diagnostic.code, diagnostic.message);
        }
        other => {
          panic!("XPASS-COMPILE {key}: native compilation unexpectedly succeeded: {other:?}");
        }
      },

      ExpectMode::XfailRun => {
        if matches!(native, RunOutcome::CompileError { .. }) {
          panic!("native compile failed for {file_name} (expected xfail-run, i.e. compile success): {native:?}");
        }

        match compare_run_outcomes(&oracle, &native, opts) {
          Ok(()) => {
            panic!("XPASS-RUN {key}: native output unexpectedly matched oracle");
          }
          Err(err) => {
            if let Some(reason) = exp.reason.as_deref() {
              println!("XFAIL-RUN {key}: {reason}");
            } else {
              println!("XFAIL-RUN {key}");
            }
            println!("  mismatch as expected: {err}");
          }
        }
      }

      ExpectMode::Skip => unreachable!("handled above"),
    }
  }
}

