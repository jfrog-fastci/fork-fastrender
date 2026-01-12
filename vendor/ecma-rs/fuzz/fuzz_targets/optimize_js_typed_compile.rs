#![no_main]

use libfuzzer_sys::fuzz_target;

// This fuzz target is only meaningful when `ecma-rs-fuzz` is built with `--features typed`.
// Keep a no-op harness so `cargo fuzz build` succeeds in default configurations.
#[cfg(not(feature = "typed"))]
fuzz_target!(|_data: &[u8]| {});

#[cfg(feature = "typed")]
mod typed {
  use super::*;
  use optimize_js::{compile_file_native_ready, NativeReadyOptions, TopLevelMode};
  use std::sync::Arc;
  use typecheck_ts::lib_support::{CacheMode, CacheOptions, CompilerOptions};
  use typecheck_ts::{FileKey, MemoryHost, Program};

  const MAX_SOURCE_BYTES: usize = 4 * 1024;

  fn fuzz_impl(data: &[u8]) {
    let data = if data.len() > MAX_SOURCE_BYTES {
      &data[..MAX_SOURCE_BYTES]
    } else {
      data
    };

    let source = String::from_utf8_lossy(data);
    let first = data.first().copied().unwrap_or(0);

    let mode = if (first & 1) != 0 {
      TopLevelMode::Module
    } else {
      TopLevelMode::Script
    };

    // Keep the typechecker lightweight: avoid pulling in the default DOM + ES lib set.
    let mut options = CompilerOptions::default();
    options.no_default_lib = true;
    // Bound internal caches so hostile inputs can't grow memory unboundedly.
    options.cache = CacheOptions {
      max_relation_cache_entries: 1024,
      max_eval_cache_entries: 1024,
      max_instantiation_cache_entries: 512,
      max_body_cache_entries: 256,
      max_def_cache_entries: 256,
      cache_shards: 1,
      mode: CacheMode::PerBody,
    };

    let mut host = MemoryHost::with_options(options);
    let file_key = FileKey::new("input.ts");
    host.insert(file_key.clone(), source.to_string());

    let program = Arc::new(Program::new(host, vec![file_key.clone()]));
    let _ = program.check();
    let Some(file_id) = program.file_id(&file_key) else {
      return;
    };

    // Toggle optimizer passes for coverage and to keep some iterations cheap.
    let opts = NativeReadyOptions {
      run_opt_passes: (first & 2) == 0,
      // Strict-native validation is part of the native-ready pipeline; toggle it so we still
      // exercise the verifier while keeping some iterations cheaper.
      verify_strict_native: (first & 4) == 0,
      ..Default::default()
    };

    let _ = compile_file_native_ready(program, file_id, mode, false, opts);
  }

  fuzz_target!(|data: &[u8]| {
    fuzz_impl(data);
  });
}
