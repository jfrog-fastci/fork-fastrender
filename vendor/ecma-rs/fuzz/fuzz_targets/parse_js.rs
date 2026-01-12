#![no_main]

use libfuzzer_sys::fuzz_target;
use parse_js::{parse_with_options_cancellable_by, Dialect, ParseOptions, SourceType};
use std::time::{Duration, Instant};

const MAX_SOURCE_BYTES: usize = 8 * 1024;

// Parser-only fuzzing: ensure we never hang on hostile inputs by using the parser's cooperative
// cancellation hook. We prefer a deterministic "step" budget and only consult wall-clock time as a
// last resort to avoid excessive `Instant::now()` overhead in hot loops.
const MAX_CANCEL_CHECKS: u32 = 200_000;
const MAX_WALL_TIME: Duration = Duration::from_millis(20);

fuzz_target!(|data: &[u8]| {
  // Cap input size to keep parsing/AST allocations bounded and make fuzz iterations cheap.
  let data = if data.len() > MAX_SOURCE_BYTES {
    &data[..MAX_SOURCE_BYTES]
  } else {
    data
  };

  let source = String::from_utf8_lossy(data);

  // Toggle module/script parsing to cover both goal symbol paths.
  let source_type = if data.first().is_some_and(|b| (b & 1) == 0) {
    SourceType::Script
  } else {
    SourceType::Module
  };
  let opts = ParseOptions {
    dialect: Dialect::Ecma,
    source_type,
  };

  let start = Instant::now();
  let mut checks: u32 = 0;

  let _ = parse_with_options_cancellable_by(&source, opts, || {
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
  });
});

