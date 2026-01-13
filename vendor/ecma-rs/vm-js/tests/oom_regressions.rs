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

fn run_oom_harness(scenario: &str, len_code_units: usize) {
  // Avoid running multiple memory-pressure subprocesses in parallel (tests run in multiple
  // threads by default).
  let _guard = OOM_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());

  let exe = env!("CARGO_BIN_EXE_oom_harness");
  let output = unsafe {
    let mut cmd = Command::new(exe);
    cmd.arg(scenario);
    cmd.arg(len_code_units.to_string());
    cmd.arg(FILLER_BYTES.to_string());

    cmd.pre_exec(|| {
      let lim = libc::rlimit {
        rlim_cur: LIMIT_AS_BYTES,
        rlim_max: LIMIT_AS_BYTES,
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
  run_oom_harness("arrayMap", 15_000_000);
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
fn module_linking_error_strings_do_not_abort_on_oom() {
  // Module linking errors may embed attacker-controlled module specifiers / export names in the
  // thrown SyntaxError message. Ensure those error strings are constructed using bounded, fallible
  // formatting so allocator OOM does not abort the process.
  run_oom_harness("moduleLink", 18_000_000);
}
