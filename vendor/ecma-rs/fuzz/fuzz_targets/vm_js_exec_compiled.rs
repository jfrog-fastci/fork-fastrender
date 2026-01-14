#![no_main]

use libfuzzer_sys::fuzz_target;
use std::borrow::Cow;
use std::fmt::Write;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use vm_js::{Agent, Budget, CompiledScript, HeapLimits, HostHooks, VmError, VmOptions};

const MAX_SOURCE_BYTES: usize = 8 * 1024;

// Keep fuzz iterations cheap and bounded. The VM is cooperative: these limits rely on the evaluator
// calling `Vm::tick()` regularly.
const VM_FUEL: u64 = 10_000;
const VM_DEADLINE: Duration = Duration::from_millis(20);

// Per-run heap limits. This is intentionally small so fuzzing finds OOM and accounting bugs quickly,
// but large enough to finish realm initialization and exercise builtins.
const HEAP_MAX_BYTES: usize = 16 * 1024 * 1024;
const HEAP_GC_THRESHOLD: usize = 8 * 1024 * 1024;

#[inline]
fn string_from_str_best_effort(s: &str) -> String {
  let mut out = String::new();
  if out.try_reserve_exact(s.len()).is_ok() {
    out.push_str(s);
  }
  out
}

fn source_text_from_bytes_best_effort(data: &[u8]) -> Option<Cow<'_, str>> {
  if let Ok(s) = std::str::from_utf8(data) {
    return Some(Cow::Borrowed(s));
  }

  // Best-effort lossy conversion that avoids `String::from_utf8_lossy`'s infallible allocation
  // (which can abort the process on OOM).
  //
  // Map each byte to a Unicode scalar in the Latin-1 range so the resulting string is always valid
  // UTF-8 and has a predictable max expansion factor.
  let mut out = String::new();
  if out.try_reserve_exact(data.len().saturating_mul(2)).is_err() {
    return None;
  }
  for &b in data {
    out.push(b as char);
  }
  Some(Cow::Owned(out))
}

fn panic_on_vm_bug(err: VmError) {
  // The fuzz harness should treat "engine bug" error variants as crashes so they are minimized and
  // preserved as libFuzzer findings.
  //
  // Many other `VmError` variants represent:
  // - expected termination (fuel/deadline/interrupt/OOM),
  // - syntax errors,
  // - JS exceptions,
  // - or unimplemented features.
  //
  // Those are not fuzz "crashes" and are ignored by this harness.
  match err {
    VmError::InvariantViolation(_) | VmError::InvalidHandle { .. } | VmError::LimitExceeded(_) => {
      panic!("vm-js bug: {err:?}");
    }
    _ => {}
  }
}

fn js_string_literal(s: &str) -> String {
  // Avoid `String::with_capacity` so the fuzz harness doesn't abort the process on allocator OOM.
  // This is best-effort: if we can't reserve, fall back to a tiny empty-string literal.
  let mut out = String::new();
  if out
    .try_reserve_exact(2usize.saturating_add(6usize.saturating_mul(s.len())))
    .is_err()
  {
    return string_from_str_best_effort("\"\"");
  }
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
        // Control characters: use `\\xNN` escapes.
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
  if quoted.is_empty() {
    // If we couldn't allocate the string literal, don't try to build the wrapper around it.
    return string_from_str_best_effort("0;");
  }
  let mut s = String::new();
  if s
    .try_reserve(2048usize.saturating_add(quoted.len()))
    .is_err()
  {
    // Best-effort: if we can't allocate the wrapper, still return a valid script.
    return string_from_str_best_effort("0;");
  }
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
  s.push_str("  try { Math.random(); } catch (e) {}\n");

  // String operations (keep allocations bounded).
  s.push_str("  try { (src + src).toUpperCase(); } catch (e) {}\n");
  s.push_str("  try { src.slice(0, 64).split(\"\"); } catch (e) {}\n");
  s.push_str("  try { src.indexOf(\"a\"); } catch (e) {}\n");

  // Promises: enqueue jobs so the host microtask checkpoint has work to drain.
  s.push_str("  try { Promise.resolve(src).then(function(v){ return v; }); } catch (e) {}\n");
  s.push_str("  try { Promise.resolve(1).then(function(x){ return x + 1; }); } catch (e) {}\n");

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

struct FuzzHostHooks;

impl HostHooks for FuzzHostHooks {
  fn microtask_checkpoint(&mut self, agent: &mut Agent) -> Result<(), VmError> {
    // Drain any queued Promise jobs so fuzzing covers Promise job execution paths too.
    //
    // Ignore errors here: the fuzz harness is primarily interested in panics and invariant
    // violations. VM termination/OOM are expected outcomes under tight budgets/heap limits.
    //
    // Important: apply a fresh per-checkpoint budget. Promise jobs can enqueue more Promise jobs,
    // so a hostile input can create an infinite microtask chain. The VM is cooperative, so
    // fuel+deadline limits are what prevent fuzzing from hanging.
    let prev_budget = agent.vm_mut().swap_budget_state(make_budget());
    let checkpoint = agent.perform_microtask_checkpoint();
    agent.vm_mut().restore_budget_state(prev_budget);
    if let Err(err) = checkpoint {
      panic_on_vm_bug(err);
    }
    Ok(())
  }
}

fn drain_microtasks(agent: &mut Agent) {
  // If compilation or execution terminated early (termination/OOM), Promise jobs may still be
  // queued. Drain/discard them before dropping the runtime so jobs can clean up persistent roots.
  let prev_budget = agent.vm_mut().swap_budget_state(make_budget());
  if let Err(err) = agent.perform_microtask_checkpoint() {
    panic_on_vm_bug(err);
  }
  agent.vm_mut().restore_budget_state(prev_budget);
}

fn compile_and_exec(agent: &mut Agent, name: &str, source: &str, hooks: &mut FuzzHostHooks) {
  // Apply the budget during compilation so we catch missed-tick hangs and respect interrupt
  // requests in the parser/early-error paths.
  let prev_budget = agent.vm_mut().swap_budget_state(make_budget());

  let script = match {
    // Borrow-split the Agent so we can pass `&mut Vm` + `&mut Heap` to the compilation API.
    let (vm, _realm, heap) = agent.vm_realm_and_heap_mut();
    CompiledScript::compile_script_with_budget(heap, vm, name, source)
  } {
    Ok(s) => s,
    Err(err) => {
      agent.vm_mut().restore_budget_state(prev_budget);
      panic_on_vm_bug(err);
      return;
    }
  };
  agent.vm_mut().restore_budget_state(prev_budget);

  if let Err(err) = agent.run_compiled_script(script, make_budget(), Some(hooks)) {
    panic_on_vm_bug(err);
  }
}

fuzz_target!(|data: &[u8]| {
  let data = if data.len() > MAX_SOURCE_BYTES {
    &data[..MAX_SOURCE_BYTES]
  } else {
    data
  };

  let Some(source) = source_text_from_bytes_best_effort(data) else {
    return;
  };

  let mut seed_bytes = [0u8; 8];
  let seed_len = data.len().min(seed_bytes.len());
  seed_bytes[..seed_len].copy_from_slice(&data[..seed_len]);
  let math_random_seed = u64::from_le_bytes(seed_bytes);

  let interrupt_flag = Arc::new(AtomicBool::new(false));
  let vm_options = VmOptions {
    max_stack_depth: 256,
    // These defaults are mostly irrelevant because we install a per-run budget before compiling and
    // running scripts, but keeping them small makes it harder for other entry points to accidentally
    // run unbounded.
    default_fuel: Some(VM_FUEL),
    default_deadline: Some(VM_DEADLINE),
    check_time_every: 50,
    math_random_seed,
    interrupt_flag: Some(interrupt_flag.clone()),
    ..VmOptions::default()
  };

  let heap_limits = HeapLimits::new(HEAP_MAX_BYTES, HEAP_GC_THRESHOLD);

  let Ok(mut agent) = Agent::with_options(vm_options, heap_limits) else {
    return;
  };
  let mut hooks = FuzzHostHooks;

  // --- Compile + execute the input directly as a classic script (HIR). ---
  if data.first().is_some_and(|b| (b & 1) != 0) {
    interrupt_flag.store(true, Ordering::Relaxed);
  }
  compile_and_exec(&mut agent, "<fuzz>", source.as_ref(), &mut hooks);
  drain_microtasks(&mut agent);

  // Clear any interrupt requested above so subsequent runs can proceed.
  agent.vm_mut().reset_interrupt();

  // --- Compile + execute a wrapper script that forces builtins + eval/Function coverage. ---
  if data.first().is_some_and(|b| (b & 2) != 0) {
    interrupt_flag.store(true, Ordering::Relaxed);
  }
  let wrapper = wrapper_script(source.as_ref());
  compile_and_exec(&mut agent, "<fuzz-wrapper>", &wrapper, &mut hooks);
  drain_microtasks(&mut agent);
});
