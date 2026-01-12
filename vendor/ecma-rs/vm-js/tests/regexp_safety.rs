use std::io;
use std::os::unix::process::CommandExt;
use std::process::Command;
use std::sync::Mutex;
use vm_js::{
  Budget, Heap, HeapLimits, JsRuntime, PropertyDescriptor, PropertyKey, PropertyKind, TerminationReason,
  Value, Vm, VmError, VmOptions,
};

static OOM_TEST_LOCK: Mutex<()> = Mutex::new(());

// Keep the child process's address space comfortably above the vm-js runtime overhead, while still
// low enough that large RegExp compilation allocations reliably hit `VmError::OutOfMemory` instead
// of aborting the process.
const LIMIT_AS_BYTES: libc::rlim_t = 192 * 1024 * 1024;
const FILLER_BYTES: usize = 120 * 1024 * 1024;

fn define_global_string(rt: &mut JsRuntime, name: &str, units: Vec<u16>) -> Result<(), VmError> {
  let s = {
    let mut scope = rt.heap_mut().scope();
    scope.alloc_string_from_u16_vec(units)?
  };

  let global = rt.realm().global_object();
  let mut scope = rt.heap_mut().scope();
  scope.push_roots(&[Value::Object(global), Value::String(s)])?;

  let key_s = scope.alloc_string(name)?;
  scope.push_root(Value::String(key_s))?;

  let desc = PropertyDescriptor {
    enumerable: true,
    configurable: true,
    kind: PropertyKind::Data {
      value: Value::String(s),
      writable: true,
    },
  };
  let key = PropertyKey::from_string(key_s);
  scope.define_property(global, key, desc)?;
  Ok(())
}

fn run_oom_harness_regexp(len_code_units: usize) {
  // Avoid running multiple memory-pressure subprocesses in parallel (tests run in multiple threads
  // by default).
  let _guard = OOM_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());

  let exe = env!("CARGO_BIN_EXE_oom_harness");
  let output = unsafe {
    let mut cmd = Command::new(exe);
    cmd.arg("regexp");
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
fn regexp_compilation_does_not_abort_on_oom() {
  // Large RegExp compilation should be fallible and return `VmError::OutOfMemory` rather than
  // aborting the process.
  run_oom_harness_regexp(2_000_000);
}

#[test]
fn regexp_compilation_respects_heap_limits() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(4 * 1024 * 1024, 4 * 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  let len = 200_000usize;
  let mut units: Vec<u16> = Vec::new();
  units.try_reserve_exact(len).map_err(|_| VmError::OutOfMemory)?;
  units.resize(len, b'a' as u16);
  define_global_string(&mut rt, "P", units)?;

  let err = rt.exec_script("new RegExp(P);").unwrap_err();
  assert!(matches!(err, VmError::OutOfMemory));
  Ok(())
}

#[test]
fn regexp_compilation_is_interruptible_by_fuel() -> Result<(), VmError> {
  let mut vm = Vm::new(VmOptions::default());
  vm.set_budget(Budget {
    fuel: Some(10),
    deadline: None,
    check_time_every: 1,
  });
  // Give compilation plenty of heap headroom so this test specifically exercises fuel/deadline
  // budgeting rather than failing the conservative heap preflight check.
  let heap = Heap::new(HeapLimits::new(64 * 1024 * 1024, 64 * 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  // Large enough that `RegExp` compilation will tick (DEFAULT_TICK_EVERY is 1024), guaranteeing the
  // low fuel budget is exhausted before compilation finishes.
  let len = 50_000usize;
  let mut units: Vec<u16> = Vec::new();
  units.try_reserve_exact(len).map_err(|_| VmError::OutOfMemory)?;
  units.resize(len, b'a' as u16);
  define_global_string(&mut rt, "P", units)?;

  let err = rt.exec_script("new RegExp(P);").unwrap_err();
  match err {
    VmError::Termination(term) => assert_eq!(term.reason, TerminationReason::OutOfFuel),
    other => panic!("expected termination, got {other:?}"),
  }

  Ok(())
}
