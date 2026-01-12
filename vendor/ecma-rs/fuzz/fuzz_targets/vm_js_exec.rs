#![no_main]

use libfuzzer_sys::fuzz_target;
use std::fmt::Write;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use vm_js::{Agent, Budget, HeapLimits, VmOptions};

const MAX_SOURCE_BYTES: usize = 8 * 1024;

// Keep fuzz iterations cheap and bounded. The VM is cooperative: these limits rely on the evaluator
// calling `Vm::tick()` regularly (it does: once per statement and once per loop iteration).
const VM_FUEL: u64 = 10_000;
const VM_DEADLINE: Duration = Duration::from_millis(20);

// Per-run heap limits. This is intentionally small so fuzzing finds OOM and accounting bugs quickly,
// but large enough to finish realm initialization and exercise builtins.
const HEAP_MAX_BYTES: usize = 16 * 1024 * 1024;
const HEAP_GC_THRESHOLD: usize = 8 * 1024 * 1024;

fn js_string_literal(s: &str) -> String {
  let mut out = String::with_capacity(s.len().saturating_add(2));
  out.push('"');
  for c in s.chars() {
    match c {
      '"' => out.push_str("\\\""),
      '\\' => out.push_str("\\\\"),
      '\n' => out.push_str("\\n"),
      '\r' => out.push_str("\\r"),
      '\t' => out.push_str("\\t"),
      // Line separator / paragraph separator are treated as line terminators in JS source.
      '\u{2028}' => out.push_str("\\u2028"),
      '\u{2029}' => out.push_str("\\u2029"),
      c if (c as u32) < 0x20 => {
        // Control characters: use `\xNN` escapes.
        write!(&mut out, "\\x{:02x}", c as u32).unwrap();
      }
      c => out.push(c),
    }
  }
  out.push('"');
  out
}

fn wrapper_script(input: &str) -> String {
  // A fixed wrapper that always parses and executes, even when `input` is not valid JS source.
  // This improves coverage for runtime builtins by using `input` as a string payload.
  //
  // NOTE: Potentially-terminating calls (`eval`, `Function`) are intentionally at the end because
  // budget termination is not JS-catchable and would prevent earlier builtins from being exercised.
  let quoted = js_string_literal(input);
  let mut s = String::new();
  s.push_str("(function(){\n");
  s.push_str("  const src = ");
  s.push_str(&quoted);
  s.push_str(";\n");

  // JSON + number parsing.
  s.push_str("  try { JSON.parse(src); } catch (e) {}\n");
  s.push_str("  try { JSON.stringify(src); } catch (e) {}\n");
  s.push_str("  try { JSON.stringify({a: src, b: [src, 1, 2, 3]}); } catch (e) {}\n");
  s.push_str("  try { parseInt(src, 0); } catch (e) {}\n");
  s.push_str("  try { parseFloat(src); } catch (e) {}\n");

  // String operations (keep allocations bounded).
  s.push_str("  try { (src + src).toUpperCase(); } catch (e) {}\n");
  s.push_str("  try { src.slice(0, 64).split(\"\"); } catch (e) {}\n");
  s.push_str("  try { src.indexOf(\"a\"); } catch (e) {}\n");

  // Dynamic parsing/execution hooks.
  s.push_str("  try { eval(src); } catch (e) {}\n");
  s.push_str("  try { (new Function(src))(); } catch (e) {}\n");

  s.push_str("})();\n");
  s
}

fn make_budget() -> Budget {
  Budget {
    fuel: Some(VM_FUEL),
    deadline: Some(Instant::now() + VM_DEADLINE),
    // Check wall-clock time regularly without making every tick pay an `Instant::now()` cost.
    check_time_every: 50,
  }
}

fuzz_target!(|data: &[u8]| {
  let data = if data.len() > MAX_SOURCE_BYTES {
    &data[..MAX_SOURCE_BYTES]
  } else {
    data
  };

  let source = String::from_utf8_lossy(data);

  let interrupt_flag = Arc::new(AtomicBool::new(false));
  let vm_options = VmOptions {
    max_stack_depth: 256,
    // These defaults are mostly irrelevant because we install a per-run budget in `Agent::run_script`,
    // but keeping them small makes it harder for other entry points to accidentally run unbounded.
    default_fuel: Some(VM_FUEL),
    default_deadline: Some(VM_DEADLINE),
    check_time_every: 50,
    interrupt_flag: Some(interrupt_flag.clone()),
    external_interrupt_flag: None,
  };

  let heap_limits = HeapLimits::new(HEAP_MAX_BYTES, HEAP_GC_THRESHOLD);

  let Ok(mut agent) = Agent::with_options(vm_options, heap_limits) else {
    return;
  };

  // --- Run the input directly as a script (parse + execute). ---
  if data.first().is_some_and(|b| (b & 1) != 0) {
    interrupt_flag.store(true, Ordering::Relaxed);
  }
  let _ = agent.run_script("<fuzz>", source.as_ref(), make_budget(), None);

  // Clear any interrupt requested above so subsequent runs can proceed.
  agent.vm_mut().reset_interrupt();

  // --- Run a wrapper script that forces builtins + eval/Function coverage. ---
  if data.first().is_some_and(|b| (b & 2) != 0) {
    interrupt_flag.store(true, Ordering::Relaxed);
  }
  let wrapper = wrapper_script(source.as_ref());
  let _ = agent.run_script("<fuzz-wrapper>", wrapper, make_budget(), None);
});

