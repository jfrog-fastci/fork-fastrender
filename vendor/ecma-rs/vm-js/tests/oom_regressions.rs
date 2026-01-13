use std::io;
use std::os::unix::process::CommandExt;
use std::process::Command;
use std::sync::Mutex;

static OOM_TEST_LOCK: Mutex<()> = Mutex::new(());

// Keep the child process's address space comfortably above the vm-js runtime overhead, while still
// low enough that the large string conversions in this test reliably hit `VmError::OutOfMemory`
// instead of aborting the process.
const LIMIT_AS_BYTES: libc::rlim_t = 192 * 1024 * 1024;
const FILLER_BYTES: usize = 120 * 1024 * 1024;

fn run_oom_harness_with_limits(
  scenario: &str,
  len_code_units: usize,
  limit_as_bytes: libc::rlim_t,
  filler_bytes: usize,
) {
  // Avoid running multiple memory-pressure subprocesses in parallel (tests run in multiple
  // threads by default).
  let _guard = OOM_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());

  let exe = env!("CARGO_BIN_EXE_oom_harness");
  let output = unsafe {
    let mut cmd = Command::new(exe);
    cmd.arg(scenario);
    cmd.arg(len_code_units.to_string());
    cmd.arg(filler_bytes.to_string());

    cmd.pre_exec(move || {
      let lim = libc::rlimit {
        rlim_cur: limit_as_bytes,
        rlim_max: limit_as_bytes,
      };
      if libc::setrlimit(libc::RLIMIT_AS, &lim) != 0 {
        return Err(io::Error::last_os_error());
      }
      Ok(())
    });

    cmd.output().expect("spawn oom_harness")
  };

  assert!(
    output.status.success(),
    "oom_harness failed: status={status}\nstdout:\n{stdout}\nstderr:\n{stderr}",
    status = output.status,
    stdout = String::from_utf8_lossy(&output.stdout),
    stderr = String::from_utf8_lossy(&output.stderr),
  );
}

fn run_oom_harness(scenario: &str, len_code_units: usize) {
  run_oom_harness_with_limits(scenario, len_code_units, LIMIT_AS_BYTES, FILLER_BYTES);
}

#[test]
fn eval_large_string_does_not_abort_on_oom() {
  // Large direct-eval string conversion should be fallible (no process abort on OOM).
  run_oom_harness("eval", 15_000_000);
}

#[test]
fn function_constructor_large_string_does_not_abort_on_oom() {
  // Large `Function(param, body)` source building should be fallible.
  run_oom_harness("function", 15_000_000);
}

#[test]
fn generator_function_constructor_large_string_does_not_abort_on_oom() {
  // `%GeneratorFunction%` (reachable via `Object.getPrototypeOf(function*(){}).constructor`) should
  // not abort when asked to build enormous source strings.
  run_oom_harness("generator", 15_000_000);
}

#[test]
fn generator_function_invocation_does_not_abort_on_oom() {
  // Calling a generator function creates a generator object with a boxed continuation. That boxing
  // must be fallible (return `VmError::OutOfMemory`), not abort the process.
  //
  // This scenario allocates a large number of generator objects; tighten headroom so it reaches OOM
  // quickly (otherwise the test can take minutes in debug builds).
  const GENERATOR_INVOKE_FILLER_BYTES: usize = 165 * 1024 * 1024;
  run_oom_harness_with_limits(
    "generator_invoke",
    0,
    LIMIT_AS_BYTES,
    GENERATOR_INVOKE_FILLER_BYTES,
  );
}

#[test]
fn number_conversion_large_string_does_not_abort_on_oom() {
  // Large `Number(string)` conversion should not abort even when the UTF-16→UTF-8 conversion
  // cannot allocate.
  run_oom_harness("number", 15_000_000);
}

#[test]
fn parse_float_large_string_does_not_abort_on_oom() {
  // Large `parseFloat(string)` should not abort under memory pressure.
  run_oom_harness("parseFloat", 25_000_000);
}

#[test]
fn regexp_compile_large_string_does_not_abort_on_oom() {
  // Large RegExp compilation should use fallible allocations and report `VmError::OutOfMemory`
  // rather than aborting the process under RLIMIT_AS pressure.
  run_oom_harness("regexp_compile", 3_000_000);
}

#[test]
fn array_map_large_length_does_not_abort_on_oom() {
  // `Array.prototype.map` formats `ToString(k)` for each index `k < length`. Ensure tight-loop index
  // key formatting is fallible and does not abort under allocator OOM.
  //
  // Use a slightly larger length to reduce total runtime: a larger backing string leaves less
  // headroom under RLIMIT_AS, so the per-iteration key allocations hit OOM sooner.
  // Keep this below the input-string allocation failure threshold (observed at ~29M code units
  // with the current RLIMIT_AS and filler settings) so the harness can reliably allocate the
  // initial `S` string before the actual test loop runs.
  run_oom_harness("arrayMap", 26_000_000);
}

#[test]
fn job_queue_enqueue_does_not_abort_on_oom() {
  // Enqueuing into the public JobQueue scaffolding should be fallible and must never abort the
  // process when the underlying VecDeque needs to grow.
  run_oom_harness("jobQueue", 0);
}

#[test]
fn job_callback_creation_does_not_abort_on_oom() {
  // Creating JobCallback records (HostMakeJobCallback) must be fallible under allocator OOM and
  // must never abort the process.
  run_oom_harness("jobCallback", 0);
}

#[test]
fn alloc_string_from_u16_vec_with_spare_capacity_does_not_abort_on_oom() {
  // Converting a UTF-16 buffer with spare capacity into a heap `JsString` should not use infallible
  // reallocations (e.g. `shrink_to_fit` / `into_boxed_slice`) that could abort the process under
  // allocator OOM.
  run_oom_harness("allocStringU16SpareCap", 20_000_000);
}

#[test]
fn throw_string_formatting_does_not_abort_on_oom() {
  // Formatting a thrown string for host-visible reporting (Agent::format_vm_error) must not abort
  // when the UTF-16→UTF-8 conversion cannot allocate under RLIMIT_AS pressure.
  run_oom_harness("throw_string_format", 15_000_000);
}

#[test]
fn get_prototype_of_proxy_chain_does_not_abort_on_oom() {
  // `Object.getPrototypeOf(proxy)` walks proxy chains and uses a `HashSet` for cycle detection.
  // Ensure allocator OOM does not abort the process.
  run_oom_harness("getPrototypeOf_proxy_chain", 0);
}

#[test]
fn set_prototype_of_cycle_check_does_not_abort_on_oom() {
  // `Object.setPrototypeOf` performs cycle detection using a `HashSet`. Ensure allocator OOM is
  // handled as `VmError::OutOfMemory` rather than aborting.
  run_oom_harness("setPrototypeOf_cycle_check", 0);
}

#[test]
fn stack_trace_formatting_does_not_abort_on_oom() {
  // Ensure host stack trace formatting (used when surfacing exceptions/terminations) is OOM-safe.
  //
  // This drives the `stackTrace` scenario, which uses a huge `source_name` held alive by captured
  // stack frames. Formatting the stack should never abort the process, even when it cannot
  // allocate enough space to render the full trace.
  run_oom_harness("stackTrace", 16 * 1024 * 1024);
}

#[test]
fn module_linking_error_strings_do_not_abort_on_oom() {
  // Module linking errors may embed attacker-controlled module specifiers / export names in the
  // thrown SyntaxError message. Ensure those error strings are constructed using bounded, fallible
  // formatting so allocator OOM does not abort the process.
  //
  // `vm-js` stores module specifiers as UTF-16 code units, so the OOM harness allocates
  // significantly more host memory per character than when this test used Rust `String`.
  // Keep the specifier large enough to stress error formatting while still fitting under the
  // RLIMIT_AS headroom.
  run_oom_harness("moduleLink", 8_000_000);
}

#[test]
fn module_graph_growth_does_not_abort_on_oom() {
  // ModuleGraph growth is attacker-driven via host module loading / dynamic `import()`. Ensure
  // graph insertion uses fallible pre-reservation and does not abort under RLIMIT_AS pressure.
  //
  // The oom harness interprets `len_code_units` as the specifier length for this scenario.
  run_oom_harness("moduleGraph", 8 * 1024);
}

#[test]
fn internal_promise_reaction_list_growth_does_not_abort_on_oom() {
  // Engine-internal Promise reaction lists (async/await plumbing) must use fallible growth and
  // surface `VmError::OutOfMemory` rather than aborting the process under allocator OOM.
  run_oom_harness("internalPromiseReactions", 0);
}

#[test]
fn label_early_error_large_label_does_not_abort_on_oom() {
  // Early-error diagnostics that embed attacker-controlled label identifiers must use bounded,
  // fallible formatting and must not abort on allocator OOM.
  run_oom_harness("labelEarlyError", 25_000_000);
}

#[test]
fn microtask_checkpoint_many_errors_does_not_abort_on_oom() {
  // `MicrotaskQueue::perform_microtask_checkpoint` collects job errors into a Rust `Vec`. Under a
  // tight RLIMIT_AS, growing that vector must use fallible reservations to avoid aborting the
  // process when many jobs fail.
  //
  // Use a slightly larger filler buffer than the string tests so the checkpoint hits allocator OOM
  // after a moderate number of failing microtasks.
  const MICROTASK_FILLER_BYTES: usize = 140 * 1024 * 1024;
  run_oom_harness_with_limits(
    "microtask_checkpoint_errors",
    10_000_000,
    LIMIT_AS_BYTES,
    MICROTASK_FILLER_BYTES,
  );
}

#[test]
fn module_get_exported_names_large_export_name_does_not_abort_on_oom() {
  // `SourceTextModuleRecord::get_exported_names_with_vm` must use fallible string copies for export
  // names. Large attacker-controlled export names should report `VmError::OutOfMemory` rather than
  // aborting the process under allocator OOM.
  run_oom_harness("moduleGetExportedNames", 15_000_000);
}

#[test]
fn global_var_decl_instantiation_large_name_does_not_abort_on_oom() {
  // GlobalDeclarationInstantiation collects and validates var-scoped names, then records them for
  // future instantiation checks. All name copies and set growth must be fallible under allocator
  // OOM (no process abort).
  //
  // Use a larger filler than the default so parsing can still succeed, but subsequent instantiation
  // work hits allocator OOM reliably.
  const GLOBAL_VAR_DECL_FILLER_BYTES: usize = 135 * 1024 * 1024;
  run_oom_harness_with_limits(
    "globalVarDecl",
    15_000_000,
    LIMIT_AS_BYTES,
    GLOBAL_VAR_DECL_FILLER_BYTES,
  );
}

#[test]
fn capture_stack_does_not_abort_on_oom() {
  // Capturing the VM stack for `ThrowWithStack` must not abort the process under allocator OOM.
  run_oom_harness("captureStack", 0);
}

#[test]
fn register_ecma_function_does_not_abort_on_oom() {
  // Growing the VM's `ecma_function_cache` must use fallible allocations (no process abort under
  // allocator OOM).
  //
  // The oom harness interprets `len_code_units` as the number of dynamic `Function` constructor
  // calls to run.
  //
  // Use a more aggressive filler buffer so we reliably hit allocator OOM quickly (otherwise this
  // test can take a very long time to complete when `Function('')` creation succeeds).
  const REGISTER_FILLER_BYTES: usize = 168 * 1024 * 1024;
  run_oom_harness_with_limits(
    "register_ecma_function",
    200_000,
    LIMIT_AS_BYTES,
    REGISTER_FILLER_BYTES,
  );
}

#[test]
fn promise_job_creation_does_not_abort_on_oom() {
  // Enqueuing a Promise job (`HostEnqueuePromiseJob`) boxes an internal job closure. Ensure this
  // allocation is fallible under RLIMIT_AS pressure (no process abort on allocator OOM).
  run_oom_harness("promiseJob", 1);
}

#[test]
fn generator_instance_creation_does_not_abort_on_oom() {
  // Creating a generator object boxes a `GeneratorContinuation`. Ensure this is a fallible
  // allocation under RLIMIT_AS pressure (no process abort on allocator OOM).
  run_oom_harness("generatorInstance", 1);
}
