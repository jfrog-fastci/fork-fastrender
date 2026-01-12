//! Fuzz the typed `optimize-js` pipeline + analysis driver.
//!
//! This target is only active when `ecma-rs-fuzz` is built with the `typed` feature, which enables
//! `optimize-js/typed` (and pulls in the TypeScript typechecker).
//!
//! Run (from the repo root) with a hard timeout (via `timeout -k`) and the repo's fuzz wrapper:
//! ```bash
//! # One-time: create a gitignored output corpus directory.
//! mkdir -p vendor/ecma-rs/fuzz/corpus/optimize_js_compile_typed
//!
//! timeout -k 10 600 bash vendor/ecma-rs/scripts/cargo_agent.sh fuzz run optimize_js_compile_typed \
//!   --features typed \
//!   fuzz/corpus/optimize_js_compile_typed -- -max_total_time=10
//! ```
#![no_main]

use libfuzzer_sys::fuzz_target;

/// Keep per-input work bounded (typechecking can be expensive).
#[cfg(feature = "typed")]
const MAX_SOURCE_BYTES: usize = 8 * 1024;

#[cfg(feature = "typed")]
fuzz_target!(|data: &[u8]| {
  let data = &data[..data.len().min(MAX_SOURCE_BYTES)];
  let source = String::from_utf8_lossy(data);

  let cfg_options = optimize_js::CompileCfgOptions {
    keep_ssa: true,
    run_opt_passes: true,
    ..Default::default()
  };

  let Ok(mut program) = optimize_js::compile_source_typed_cfg_options(
    source.as_ref(),
    optimize_js::TopLevelMode::Module,
    false,
    cfg_options,
  ) else {
    // Parse/type/lowering errors and TypeScript diagnostics are expected for random input.
    return;
  };

  let _ = optimize_js::analysis::annotate_program(&mut program);
});

// When built without `--features typed`, keep the fuzz target buildable (and listable) while
// doing no work.
#[cfg(not(feature = "typed"))]
fuzz_target!(|_data: &[u8]| {});
