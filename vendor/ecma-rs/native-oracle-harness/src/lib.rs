//! Test harness for comparing native execution against the `vm-js` oracle.
//!
//! This crate is intentionally small. Its primary job is to:
//! - Load TypeScript fixtures.
//! - Erase TypeScript-only syntax to JavaScript (TS→JS "erasure").
//! - Execute the erased JavaScript in the oracle runtime.
//!
//! The TS→JS step uses the `emit-js` erasure emitter by default. When that
//! emitter encounters unsupported syntax, consumers can enable the
//! `optimize-js-fallback` feature to fall back to the heavier `optimize-js`
//! compile+decompile path.

use emit_js::{EmitOptions, Emitter};
use parse_js::{Dialect, ParseOptions, SourceType};

#[derive(Debug)]
pub enum TsToJsError {
  Parse(parse_js::error::SyntaxError),
  Emit(emit_js::JsEmitError),
  #[cfg(feature = "optimize-js-fallback")]
  Optimize(Vec<optimize_js::Diagnostic>),
  #[cfg(feature = "optimize-js-fallback")]
  OptimizeEmit(optimize_js::ProgramToJsError),
}

impl std::fmt::Display for TsToJsError {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    match self {
      TsToJsError::Parse(err) => write!(f, "{err}"),
      TsToJsError::Emit(err) => write!(f, "emit-js TS→JS erasure failed: {err:?}"),
      #[cfg(feature = "optimize-js-fallback")]
      TsToJsError::Optimize(diagnostics) => write!(
        f,
        "optimize-js TS→JS fallback failed with {} diagnostic(s)",
        diagnostics.len()
      ),
      #[cfg(feature = "optimize-js-fallback")]
      TsToJsError::OptimizeEmit(err) => write!(f, "optimize-js TS→JS fallback emit failed: {err}"),
    }
  }
}

impl std::error::Error for TsToJsError {}

/// Erase TypeScript-only syntax from `source`, returning JavaScript that can be
/// executed by the oracle VM.
///
/// This is intentionally a best-effort API:
/// - It first attempts to use `emit-js`'s TS→JS erasure emitter.
/// - If emission fails and the `optimize-js-fallback` feature is enabled, it
///   falls back to `optimize-js`'s decompiler, which supports a wider range of
///   syntax (but is significantly heavier).
pub fn erase_typescript_to_js(source: &str) -> Result<String, TsToJsError> {
  let ast = parse_js::parse_with_options(
    source,
    ParseOptions {
      dialect: Dialect::Ts,
      source_type: SourceType::Script,
    },
  )
  .map_err(TsToJsError::Parse)?;

  let mut emitter = Emitter::new(EmitOptions::minified());
  match emit_js::emit_js_top_level(&mut emitter, ast.stx.as_ref()) {
    Ok(()) => Ok(String::from_utf8(emitter.into_bytes()).expect("emitted JS is UTF-8")),
    Err(err) => erase_with_optimize_js_fallback(source, TsToJsError::Emit(err)),
  }
}

#[cfg(feature = "optimize-js-fallback")]
fn erase_with_optimize_js_fallback(source: &str, _original: TsToJsError) -> Result<String, TsToJsError> {
  use optimize_js::{compile_source, program_to_js, DecompileOptions, TopLevelMode};

  let program =
    compile_source(source, TopLevelMode::Script, false).map_err(TsToJsError::Optimize)?;
  let bytes = program_to_js(&program, &DecompileOptions::default(), EmitOptions::minified())
    .map_err(TsToJsError::OptimizeEmit)?;
  Ok(String::from_utf8(bytes).expect("optimize-js emits UTF-8"))
}

#[cfg(not(feature = "optimize-js-fallback"))]
fn erase_with_optimize_js_fallback(_source: &str, original: TsToJsError) -> Result<String, TsToJsError> {
  Err(original)
}

#[cfg(test)]
mod tests {
  use super::erase_typescript_to_js;
  use std::path::{Path, PathBuf};

  fn fixtures_dir() -> PathBuf {
    let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    manifest_dir
      .parent()
      .expect("crate should live under vendor/ecma-rs/")
      .join("fixtures/native_oracle")
  }

  #[test]
  fn fixtures_erase_and_execute_in_oracle() {
    let dir = fixtures_dir();
    let mut fixtures: Vec<PathBuf> = std::fs::read_dir(&dir)
      .unwrap_or_else(|err| panic!("failed to read fixture dir {dir:?}: {err}"))
      .filter_map(|entry| entry.ok().map(|entry| entry.path()))
      .filter(|path| path.extension().is_some_and(|ext| ext == "ts"))
      .collect();
    fixtures.sort();

    assert!(
      !fixtures.is_empty(),
      "expected at least one fixture under {dir:?}"
    );

    for fixture in fixtures {
      let source = std::fs::read_to_string(&fixture)
        .unwrap_or_else(|err| panic!("failed to read fixture {fixture:?}: {err}"));
      let js = erase_typescript_to_js(&source)
        .unwrap_or_else(|err| panic!("failed to erase fixture {fixture:?}: {err}"));

      runtime_js::execute_script(
        &js,
        vm_js::HeapLimits::new(4 * 1024 * 1024, 2 * 1024 * 1024),
        vm_js::Budget::unlimited(1000),
      )
      .unwrap_or_else(|err| panic!("oracle execution failed for {fixture:?}: {err:?}\nJS:\n{js}"));
    }
  }
}
