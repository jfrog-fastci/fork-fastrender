use crate::js::vm_error_format;
use crate::js::ConsoleSink;
use std::sync::Arc;

/// Create a [`ConsoleSink`] that prints JavaScript console output to stderr.
///
/// This is intended for local debugging when structured diagnostics collection is disabled.
/// Output is intentionally lossy/bounded (via [`vm_error_format::format_console_arguments_limited`])
/// so it is safe to enable on untrusted pages.
pub fn stderr_console_sink() -> ConsoleSink {
  // Delegate to the internal helper used by other embeddings so stderr formatting stays
  // consistent across the codebase.
  vm_error_format::stderr_console_sink()
}

/// Combine two [`ConsoleSink`]s into one by invoking both in order.
pub fn fanout_console_sink(a: ConsoleSink, b: ConsoleSink) -> ConsoleSink {
  Arc::new(move |level, heap, args| {
    a(level, heap, args);
    b(level, heap, args);
  })
}

#[cfg(test)]
mod tests {
  use super::fanout_console_sink;
  use crate::api::ConsoleMessageLevel;
  use crate::js::ConsoleSink;
  use std::sync::atomic::{AtomicUsize, Ordering};
  use std::sync::Arc;
  use vm_js::{Heap, HeapLimits};

  #[test]
  fn fanout_console_sink_invokes_both_sinks() {
    let a_calls = Arc::new(AtomicUsize::new(0));
    let b_calls = Arc::new(AtomicUsize::new(0));

    let a: ConsoleSink = {
      let a_calls = Arc::clone(&a_calls);
      Arc::new(move |_level, _heap, _args| {
        a_calls.fetch_add(1, Ordering::Relaxed);
      })
    };

    let b: ConsoleSink = {
      let b_calls = Arc::clone(&b_calls);
      Arc::new(move |_level, _heap, _args| {
        b_calls.fetch_add(1, Ordering::Relaxed);
      })
    };

    let sink = fanout_console_sink(a, b);

    let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
    sink(ConsoleMessageLevel::Log, &mut heap, &[]);

    assert_eq!(a_calls.load(Ordering::Relaxed), 1);
    assert_eq!(b_calls.load(Ordering::Relaxed), 1);
  }
}
