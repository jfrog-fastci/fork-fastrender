//! Fuzz `optimize-js` compilation + analysis driver.
//!
//! Run (from the repo root) with CPU/memory limits:
//! ```bash
//! timeout -k 10 600 bash vendor/ecma-rs/scripts/run_limited.sh --as 16G -- \
//!   cargo fuzz run optimize_js_compile
//! ```
#![no_main]

use libfuzzer_sys::fuzz_target;
use optimize_js::{analysis, compile_source_with_cfg_options, CompileCfgOptions, TopLevelMode};

/// Keep per-input work bounded.
const MAX_SOURCE_BYTES: usize = 16 * 1024;

fuzz_target!(|data: &[u8]| {
  let data = &data[..data.len().min(MAX_SOURCE_BYTES)];
  let source = String::from_utf8_lossy(data);

  let cfg_options = CompileCfgOptions {
    keep_ssa: true,
    run_opt_passes: true,
  };

  let Ok(mut program) =
    compile_source_with_cfg_options(source.as_ref(), TopLevelMode::Module, false, cfg_options)
  else {
    // Parse/type/lowering errors are expected for random input.
    return;
  };

  // Exercise the whole-program analysis driver, including instruction metadata annotations.
  let _ = analysis::annotate_program(&mut program);
});

