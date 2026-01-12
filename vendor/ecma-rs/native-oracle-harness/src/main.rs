use native_oracle_harness::expectations::{load_expectations, ExpectMode, FixtureExpectation};
use native_oracle_harness::fixtures::{
  discover_native_oracle_fixtures, run_expectation_suite, ExpectationSuiteOptions, FixtureKind,
};
use native_oracle_harness::{run_fixture_ts_module_dir, run_fixture_ts_with_name};
use std::collections::HashSet;
use std::fs;
use std::path::PathBuf;

fn native_oracle_fixture_dir() -> PathBuf {
  PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../fixtures/native_oracle")
}

fn native_compare_dir() -> PathBuf {
  PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../fixtures/native_compare")
}

fn expectations_path() -> PathBuf {
  native_compare_dir().join("expectations.toml")
}

fn usage() -> &'static str {
  "\
native-oracle-harness

Runs the TypeScript oracle fixtures under `vendor/ecma-rs/fixtures/native_oracle/` by default.

Helpers for native-vs-oracle expectation management:
  --list
      List effective expectations for fixtures under `fixtures/native_compare/`.

  --set-expectation <fixture> <mode> [--reason <text>]
      Set an expectation for a fixture in `fixtures/native_compare/expectations.toml`.
      Valid modes: pass | xfail-compile | xfail-run | skip
      Use <fixture> = default to update the [default] section.
"
}

fn toml_escape_string(s: &str) -> String {
  // Basic TOML string escape (double-quoted).
  let mut out = String::with_capacity(s.len() + 2);
  for ch in s.chars() {
    match ch {
      '\\' => out.push_str("\\\\"),
      '"' => out.push_str("\\\""),
      '\n' => out.push_str("\\n"),
      '\r' => out.push_str("\\r"),
      '\t' => out.push_str("\\t"),
      other => out.push(other),
    }
  }
  out
}

fn parse_fixture_order(raw: &str) -> Vec<String> {
  let mut out = Vec::new();
  for line in raw.lines() {
    let line = line.trim();
    let Some(rest) = line.strip_prefix("[fixture.") else {
      continue;
    };
    let Some(name) = rest.strip_suffix(']') else {
      continue;
    };
    if name.is_empty() {
      continue;
    }
    if !out.iter().any(|s| s == name) {
      out.push(name.to_string());
    }
  }
  out
}

fn render_expectations_toml(
  default: &FixtureExpectation,
  fixtures: &[(String, FixtureExpectation)],
) -> String {
  let mut out = String::new();
  out.push_str(
    "# Expectations for `native-oracle-harness` native-vs-oracle fixture comparisons.\n\
#\n\
# Each fixture can be assigned a mode:\n\
# - \"pass\": native output must match the oracle output\n\
# - \"xfail-compile\": native compilation is expected to fail (known gap)\n\
# - \"xfail-run\": native compilation is expected to succeed, but runtime mismatch/termination is expected (known gap)\n\
# - \"skip\": do not run this fixture\n\
#\n\
# The `[default]` section provides the default mode for any fixture that does not have an explicit\n\
# `[fixture.<name>]` entry.\n\n",
  );

  out.push_str("[default]\n");
  out.push_str(&format!("mode = \"{}\"\n", default.mode));
  if let Some(reason) = default.reason.as_deref() {
    out.push_str(&format!("reason = \"{}\"\n", toml_escape_string(reason)));
  }

  for (name, exp) in fixtures {
    out.push('\n');
    out.push_str(&format!("[fixture.{name}]\n"));
    out.push_str(&format!("mode = \"{}\"\n", exp.mode));
    if let Some(reason) = exp.reason.as_deref() {
      out.push_str(&format!("reason = \"{}\"\n", toml_escape_string(reason)));
    }
  }

  out
}

fn list_expectations() -> Result<(), Box<dyn std::error::Error>> {
  let dir = native_compare_dir();
  let expectations_path = expectations_path();
  let expectations = load_expectations(&expectations_path);
  let default = expectations
    .get("default")
    .cloned()
    .unwrap_or_else(FixtureExpectation::pass);

  let mut fixture_names: Vec<String> = fs::read_dir(&dir)?
    .filter_map(|entry| entry.ok().map(|e| e.path()))
    .filter(|p| p.extension().is_some_and(|ext| ext == "ts" || ext == "tsx"))
    .filter_map(|p| p.file_stem().and_then(|s| s.to_str()).map(|s| s.to_string()))
    .collect();
  fixture_names.sort();

  println!(
    "default\t{}\t{}",
    default.mode,
    default.reason.as_deref().unwrap_or("")
  );

  let mut seen = HashSet::<String>::new();
  for name in fixture_names {
    let exp = expectations.get(&name).cloned().unwrap_or_else(|| default.clone());
    println!("{name}\t{}\t{}", exp.mode, exp.reason.as_deref().unwrap_or(""));
    seen.insert(name);
  }

  // Also list manifest entries that don't currently have a `*.ts` fixture file.
  let mut extra: Vec<_> = expectations
    .iter()
    .filter(|(k, _)| k.as_str() != "default" && !seen.contains(*k))
    .map(|(k, v)| (k.clone(), v.clone()))
    .collect();
  extra.sort_by(|(a, _), (b, _)| a.cmp(b));
  for (name, exp) in extra {
    println!(
      "{name}\t{}\t{}  (no fixture file)",
      exp.mode,
      exp.reason.as_deref().unwrap_or("")
    );
  }

  Ok(())
}

fn set_expectation(
  fixture: &str,
  mode: ExpectMode,
  reason: Option<Option<String>>,
) -> Result<(), Box<dyn std::error::Error>> {
  let path = expectations_path();
  let raw = fs::read_to_string(&path).unwrap_or_default();
  let order = parse_fixture_order(&raw);

  let mut expectations = load_expectations(&path);
  let mut exp = expectations
    .get(fixture)
    .cloned()
    .unwrap_or_else(FixtureExpectation::pass);
  exp.mode = mode;
  if let Some(reason) = reason {
    exp.reason = reason;
  }
  expectations.insert(fixture.to_string(), exp);

  let default = expectations
    .get("default")
    .cloned()
    .unwrap_or_else(FixtureExpectation::pass);

  // Preserve existing ordering when possible; append new entries deterministically.
  let mut fixture_entries: Vec<(String, FixtureExpectation)> = Vec::new();
  let mut seen = HashSet::<String>::new();
  for name in order {
    if let Some(exp) = expectations.get(&name).cloned() {
      fixture_entries.push((name.clone(), exp));
      seen.insert(name);
    }
  }

  let mut remaining: Vec<_> = expectations
    .into_iter()
    .filter(|(k, _)| k.as_str() != "default" && !seen.contains(k))
    .collect();
  remaining.sort_by(|(a, _), (b, _)| a.cmp(b));
  fixture_entries.extend(remaining);

  fs::create_dir_all(path.parent().unwrap())?;
  fs::write(&path, render_expectations_toml(&default, &fixture_entries))?;
  Ok(())
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
  let mut args = std::env::args().skip(1);
  match args.next().as_deref() {
    Some("--help") | Some("-h") => {
      print!("{}", usage());
      return Ok(());
    }
    Some("--list") => return list_expectations(),
    Some("--set-expectation") => {
      let fixture = args
        .next()
        .ok_or("--set-expectation requires <fixture>")?;
      if fixture != "default" && fixture.contains('.') {
        return Err("fixture names must not contain '.' (TOML dotted table segments)".into());
      }
      let mode_raw = args.next().ok_or("--set-expectation requires <mode>")?;
      let mode = ExpectMode::parse(&mode_raw)
        .ok_or("--set-expectation <mode> must be one of pass|xfail-compile|xfail-run|skip")?;

      // Optional: `--reason <text...>`
      let mut reason_override = None;
      if let Some(flag) = args.next() {
        if flag != "--reason" {
          return Err(format!("unknown flag after <mode>: {flag}").into());
        }
        let rest: Vec<String> = args.collect();
        let joined = rest.join(" ");
        reason_override = Some(if joined.is_empty() { None } else { Some(joined) });
      }

      return set_expectation(&fixture, mode, reason_override);
    }
    Some(other) => {
      return Err(format!("unknown argument {other:?}\n\n{}", usage()).into());
    }
    None => {}
  }

  let dir = native_oracle_fixture_dir();
  let cases: Vec<_> = discover_native_oracle_fixtures(&dir)
    .into_iter()
    .filter(|case| matches!(case.kind, FixtureKind::Observe | FixtureKind::ObserveModuleDir))
    .collect();

  if cases.is_empty() {
    return Err(format!("expected at least one TS/TSX fixture under {}", dir.display()).into());
  }

  let report = run_expectation_suite(
    &cases,
    |case| match case.kind {
      FixtureKind::Observe => run_fixture_ts_with_name(&case.path.to_string_lossy(), &case.source),
      FixtureKind::ObserveModuleDir => run_fixture_ts_module_dir(&case.path),
      FixtureKind::PromiseReturn => unreachable!("promise-return fixtures are filtered out"),
    },
    ExpectationSuiteOptions::default(),
  );

  for case in &cases {
    match report.failure_for_path(&case.path) {
      Some(failure) => eprintln!("{}", failure.render()),
      None => println!("ok {}", case.path.display()),
    }
  }

  if report.is_success() {
    Ok(())
  } else {
    Err(format!("{} fixture(s) failed", report.failed).into())
  }
}
