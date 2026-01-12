#![no_main]

use libfuzzer_sys::fuzz_target;
use optimize_js::analysis::driver::{annotate_escape_and_ownership, analyze_cfg};
use optimize_js::{CompileCfgOptions, Program, TopLevelMode};
use parse_js::{parse_with_options_cancellable_by, Dialect, ParseOptions, SourceType};
use std::time::{Duration, Instant};

const MAX_SOURCE_BYTES: usize = 8 * 1024;

// Keep parsing and compilation bounded on hostile inputs. We prefer a deterministic "step" budget
// and only consult wall-clock time occasionally to avoid excessive `Instant::now()` overhead in
// hot loops.
const MAX_CANCEL_CHECKS: u32 = 200_000;
const MAX_WALL_TIME: Duration = Duration::from_millis(30);

// Limit how much analysis work we perform after compilation so fuzz iterations stay cheap.
const MAX_ANALYZED_FUNCTIONS: usize = 4;
const MAX_CFG_BLOCKS_FOR_ANALYSIS: usize = 256;

fuzz_target!(|data: &[u8]| {
  // Cap input size to keep parse/compile allocations bounded.
  let data = if data.len() > MAX_SOURCE_BYTES {
    &data[..MAX_SOURCE_BYTES]
  } else {
    data
  };

  let source = String::from_utf8_lossy(data);
  let first = data.first().copied().unwrap_or(0);

  let source_type = if (first & 1) == 0 {
    SourceType::Script
  } else {
    SourceType::Module
  };

  let mode = match source_type {
    SourceType::Module => TopLevelMode::Module,
    SourceType::Script => {
      if (first & 2) == 0 {
        TopLevelMode::Global
      } else {
        TopLevelMode::Script
      }
    }
  };

  let start = Instant::now();
  let mut checks: u32 = 0;

  let opts = ParseOptions {
    dialect: Dialect::Ecma,
    source_type,
  };

  let Ok(ast) = parse_with_options_cancellable_by(&source, opts, || {
    checks = checks.wrapping_add(1);

    // Deterministic-ish step limit.
    if checks >= MAX_CANCEL_CHECKS {
      return true;
    }

    // Wall-clock backstop.
    if checks % 1024 == 0 && start.elapsed() >= MAX_WALL_TIME {
      return true;
    }

    false
  }) else {
    return;
  };

  let cfg_options = CompileCfgOptions {
    keep_ssa: (first & 4) != 0,
    run_opt_passes: (first & 8) == 0,
    ..Default::default()
  };

  let Ok(program) = Program::compile_with_cfg_options(ast, mode, false, cfg_options) else {
    return;
  };

  // Exercise a subset of analyses (range/nullability/encoding) on a bounded set of CFGs.
  for function in std::iter::once(&program.top_level)
    .chain(program.functions.iter())
    .take(MAX_ANALYZED_FUNCTIONS)
  {
    let cfg = function.analyzed_cfg();
    if cfg.bblocks.all().count() > MAX_CFG_BLOCKS_FOR_ANALYSIS {
      continue;
    }
    let _ = analyze_cfg(cfg);
  }

  // Also re-run escape/ownership annotation on a cloned CFG. This covers native-AOT-critical
  // metadata propagation without mutating the compiled program.
  let cfg = program.top_level.analyzed_cfg();
  if cfg.bblocks.all().count() <= MAX_CFG_BLOCKS_FOR_ANALYSIS {
    let mut cloned = cfg.clone();
    let _ = annotate_escape_and_ownership(&mut cloned, &program.top_level.params);
  }
});
