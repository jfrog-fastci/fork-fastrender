use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Clone, Debug)]
struct Cc {
  program: String,
  args: Vec<String>,
}

impl Cc {
  fn new(program: impl Into<String>) -> Self {
    Self {
      program: program.into(),
      args: Vec::new(),
    }
  }

  fn with_args(program: impl Into<String>, args: Vec<String>) -> Self {
    Self {
      program: program.into(),
      args,
    }
  }
}

fn find_c_compiler() -> Option<Cc> {
  // Prefer $CC when set (common in CI / cross toolchains).
  if let Ok(cc) = std::env::var("CC") {
    let parts: Vec<&str> = cc.split_whitespace().collect();
    if let Some((program, args)) = parts.split_first() {
      return Some(Cc::with_args(
        (*program).to_string(),
        args.iter().map(|s| (*s).to_string()).collect(),
      ));
    }
  }

  // Ubuntu images usually provide `cc`. Fall back to clang/gcc when needed.
  for candidate in ["cc", "clang", "gcc"] {
    if Command::new(candidate)
      .arg("--version")
      .stdout(std::process::Stdio::null())
      .stderr(std::process::Stdio::null())
      .status()
      .is_ok_and(|s| s.success())
    {
      return Some(Cc::new(candidate));
    }
  }

  None
}

fn workspace_root() -> PathBuf {
  // runtime-native/ is a workspace member; workspace root is its parent.
  Path::new(env!("CARGO_MANIFEST_DIR"))
    .parent()
    .expect("runtime-native should live at <workspace>/runtime-native")
    .to_path_buf()
}

fn target_dir() -> PathBuf {
  std::env::var_os("CARGO_TARGET_DIR")
    .map(PathBuf::from)
    .unwrap_or_else(|| workspace_root().join("target"))
}

fn find_staticlib(target_dir: &Path, profile: &str) -> PathBuf {
  let direct = target_dir.join(profile).join("libruntime_native.a");
  let mut newest: Option<(std::time::SystemTime, PathBuf)> = fs::metadata(&direct)
    .and_then(|meta| meta.modified())
    .ok()
    .map(|mtime| (mtime, direct.clone()));

  let deps_dir = target_dir.join(profile).join("deps");
  if let Ok(entries) = fs::read_dir(&deps_dir) {
    for entry in entries.flatten() {
      let path = entry.path();
      let Some(file_name) = path.file_name().and_then(|s| s.to_str()) else {
        continue;
      };
      if !(file_name.starts_with("libruntime_native") && file_name.ends_with(".a")) {
        continue;
      }

      let mtime = fs::metadata(&path)
        .and_then(|meta| meta.modified())
        .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
      match newest {
        Some((best, _)) if mtime <= best => {}
        _ => newest = Some((mtime, path)),
      }
    }
  }

  if let Some((_, path)) = newest {
    return path;
  }

  panic!(
    "failed to find runtime-native staticlib at {} (checked {} and {})",
    target_dir.display(),
    direct.display(),
    deps_dir.display()
  );
}

#[test]
fn c_can_link_and_call_runtime_native() {
  if !cfg!(unix) {
    eprintln!("skipping: C link smoke test only runs on unix-like targets");
    return;
  }

  let Some(cc) = find_c_compiler() else {
    eprintln!("skipping: no C compiler (`cc`/`clang`/`gcc`) available");
    return;
  };

  let tmp = tempfile::tempdir().expect("create temp dir");

  // Use the staticlib produced by the outer `cargo test` build to avoid re-invoking Cargo from
  // within the test (slow and risks deadlocking on the target-dir lock).
  let profile = if cfg!(debug_assertions) { "debug" } else { "release" };
  let staticlib = find_staticlib(&target_dir(), profile);

  let c_path = tmp.path().join("smoke.c");
  let bin_path = tmp.path().join("smoke");

 fs::write(
     &c_path,
     r#"
 #define _POSIX_C_SOURCE 200809L
 #include "runtime_native.h"
 #include <stdlib.h>
 #include <string.h>
 #include <time.h>
 #include <unistd.h>
 static void sleep_us(long us) {
   struct timespec ts;
   ts.tv_sec = us / 1000000;
  ts.tv_nsec = (us % 1000000) * 1000;
  nanosleep(&ts, (struct timespec*)0);
}

static void set_int(uint8_t* data) {
  int* flag = (int*)data;
  *flag = 1;
}

static void blocking_task(uint8_t* data, LegacyPromiseRef promise) {
  (void)data;
  // Ensure the main thread has time to enter `rt_async_poll_legacy` so this test
  // exercises the cross-thread wake-up path from the blocking pool.
  sleep_us(50 * 1000);
  rt_promise_resolve_legacy(promise, (ValueRef)0);
}

static void par_for_body(size_t i, uint8_t* data) {
  uint32_t* out = (uint32_t*)data;
  out[i] = (uint32_t)(i * 3u + 1u);
}

typedef struct NativeAsyncSmokeCoro {
  Coroutine header;
  int* ran;
  int* destroyed;
} NativeAsyncSmokeCoro;

static CoroutineStep native_async_smoke_resume(Coroutine* coro) {
  NativeAsyncSmokeCoro* c = (NativeAsyncSmokeCoro*)coro;
  if (c->ran) {
    *c->ran = 1;
  }
  if (coro->promise == (PromiseRef)0) {
    if (c->ran) {
      *c->ran = 2;
    }
    return (CoroutineStep){RT_CORO_STEP_COMPLETE, (PromiseRef)0};
  }
  if (!rt_promise_try_fulfill(coro->promise)) {
    if (c->ran) {
      *c->ran = 3;
    }
  }
  return (CoroutineStep){RT_CORO_STEP_COMPLETE, (PromiseRef)0};
}

static void native_async_smoke_destroy(CoroutineRef coro) {
  // Stack-owned coroutine frames must never be destroyed by the runtime.
  NativeAsyncSmokeCoro* c = (NativeAsyncSmokeCoro*)coro;
  if (c->destroyed) {
    *c->destroyed = 1;
  }
}

static const CoroutineVTable NATIVE_ASYNC_SMOKE_VTABLE = {
  .resume = native_async_smoke_resume,
  .destroy = native_async_smoke_destroy,
  // Use conservative values: this smoke test treats PromiseHeader as opaque.
  .promise_size = 64,
  .promise_align = 16,
  // Promise allocation uses `rt_alloc` and therefore requires a valid registered shape id.
  .promise_shape_id = 2,
  .abi_version = RT_ASYNC_ABI_VERSION,
  .reserved = {0, 0, 0, 0},
};

static void native_async_heap_destroy(CoroutineRef coro) {
  NativeAsyncSmokeCoro* c = (NativeAsyncSmokeCoro*)coro;
  if (c->destroyed) {
    *c->destroyed = 1;
  }
  free(c);
}

static const CoroutineVTable NATIVE_ASYNC_HEAP_VTABLE = {
  .resume = native_async_smoke_resume,
  .destroy = native_async_heap_destroy,
  .promise_size = 64,
  .promise_align = 16,
  .promise_shape_id = 2,
  .abi_version = RT_ASYNC_ABI_VERSION,
  .reserved = {0, 0, 0, 0},
};

int main(void) {
  rt_thread_init(0);
  // Ensure strict-await configuration entrypoint is present/callable.
  rt_async_set_strict_await_yields(false);
  // Ensure limit/error reporting helpers are present/callable.
  rt_async_set_limits(100000, 100000);
  char* no_error = rt_async_take_last_error();
  if (no_error != (char*)0) {
    rt_async_free_c_string(no_error);
    rt_thread_deinit();
    return 3;
  }
  // Should be a no-op for NULL pointers.
  rt_async_free_c_string(no_error);

  // Touch the RT_THREAD TLS symbol so this smoke test also verifies that the
  // runtime provides it for native codegen.
  RT_THREAD = (Thread*)0;

  // Global root registration (word-sized slot).
  static size_t global_root = 0;
  rt_global_root_register(&global_root);
  rt_global_root_unregister(&global_root);

  // Shape table is 1-indexed. Provide:
  // - shape 1: small leaf object used by the pinned-allocation smoke,
  // - shape 2: opaque promise allocation used by the native async smoke.
  static const RtShapeDescriptor kShapes[2] = {
    {
      .size = 16,
      .align = 16,
      .flags = 0,
      .ptr_offsets = (const uint32_t*)0,
      .ptr_offsets_len = 0,
      .reserved = 0,
    },
    {
      .size = 64,
      .align = 16,
      .flags = 0,
      .ptr_offsets = (const uint32_t*)0,
      .ptr_offsets_len = 0,
      .reserved = 0,
    },
  };
  rt_register_shape_table(kShapes, 2);

  RtShapeId shape = (RtShapeId)1;
  uint8_t* pinned = rt_alloc_pinned(16, shape);
  (void)pinned;

  // Warm up the blocking pool. The first `rt_spawn_blocking` call may need to
  // spawn a full worker pool, which can take long enough for a short timeout to
  // expire on slow/contended machines. This smoke test is about wakeups
  // (promise settlement waking a blocked poll), not thread pool initialization.
  int warm_settled = 0;
  LegacyPromiseRef warm = rt_spawn_blocking(blocking_task, (uint8_t*)0);
  rt_promise_then_legacy(warm, set_int, (uint8_t*)&warm_settled);
  for (int i = 0; i < 10000 && !warm_settled; i++) {
    rt_async_poll_legacy();
    // On some platforms `rt_async_poll_legacy` can return quickly when idle.
    // Avoid spinning too fast: give the blocking worker a chance to run.
    if (!warm_settled) {
      sleep_us(1000);
    }
  }
  if (!warm_settled) {
    rt_thread_deinit();
    return 1;
  }

  // Smoke test: resolve a promise from a blocking worker and run its continuation on the
  // event loop thread, waking the poll loop promptly (before the timer fires).
  //
  // Note: the blocking pool spins up worker threads on first use; give it enough slack so the
  // test isn't flaky under contention.
  int timer_fired = 0;
  TimerId t = rt_set_timeout(set_int, (uint8_t*)&timer_fired, 2000);

  int settled = 0;
  LegacyPromiseRef p = rt_spawn_blocking(blocking_task, (uint8_t*)0);
  rt_promise_then_legacy(p, set_int, (uint8_t*)&settled);
  // Drive the event loop until the promise settles.
  //
  // Under heavy CI load, the blocking worker may not run immediately. That's OK: this is a C
  // link smoke test, not a latency test.
  for (int i = 0; i < 5000 && !settled && !timer_fired; i++) {
    rt_async_poll_legacy();
    if (!settled && !timer_fired) {
      sleep_us(1000);
    }
  }
  rt_clear_timer(t);
  if (!settled) {
    rt_thread_deinit();
    return 10;
  }
  if (timer_fired) {
    // Event loop did not wake promptly (likely blocked until timeout).
    rt_thread_deinit();
    return 1;
  }

  enum { N = 4096 };
  uint32_t out[N];
  for (size_t i = 0; i < N; i++) {
    out[i] = 0;
  }
  rt_parallel_for(0, N, par_for_body, (uint8_t*)out);
  for (size_t i = 0; i < N; i++) {
    if (out[i] != (uint32_t)(i * 3u + 1u)) {
      rt_thread_deinit();
      return 2;
    }
  }

  // Native async ABI: spawn a coroutine via CoroutineId (persistent handle id).
  //
  // This is stack-owned and must complete synchronously; the runtime should *not*
  // call `destroy`, but it must still free the CoroutineId handle.
  int native_async_ran = 0;
  int native_async_destroyed = 0;
  NativeAsyncSmokeCoro native_async = {
    .header =
      {
        .vtable = &NATIVE_ASYNC_SMOKE_VTABLE,
        .promise = (PromiseRef)0,
        .next_waiter = (Coroutine*)0,
        .flags = 0,
      },
    .ran = &native_async_ran,
    .destroyed = &native_async_destroyed,
  };
  CoroutineId native_async_id = rt_handle_alloc((GcPtr)&native_async);
  PromiseRef native_promise = rt_async_spawn(native_async_id);
  if (native_promise == (PromiseRef)0) {
    rt_thread_deinit();
    return 30;
  }
  if (native_async_destroyed != 0) {
    rt_thread_deinit();
    return 31;
  }
  if (native_async_ran != 1) {
    rt_thread_deinit();
    return 32;
  }
  if (native_promise != native_async.header.promise) {
    rt_thread_deinit();
    return 33;
  }
  if (rt_promise_try_fulfill(native_promise)) {
    rt_thread_deinit();
    return 34;
  }
  if (rt_handle_load((HandleId)native_async_id) != (GcPtr)0) {
    rt_thread_deinit();
    return 35;
  }
  // Blocking wait helper should return immediately for already-settled promises.
  rt_async_block_on(native_promise);

  // Deferred spawn: must schedule the first resume as a microtask.
  int deferred_ran = 0;
  int deferred_destroyed = 0;
  NativeAsyncSmokeCoro* deferred_coro = (NativeAsyncSmokeCoro*)malloc(sizeof(NativeAsyncSmokeCoro));
  if (!deferred_coro) {
    rt_thread_deinit();
    return 36;
  }
  *deferred_coro = (NativeAsyncSmokeCoro){
    .header =
      {
        .vtable = &NATIVE_ASYNC_HEAP_VTABLE,
        .promise = (PromiseRef)0,
        .next_waiter = (Coroutine*)0,
        .flags = CORO_FLAG_RUNTIME_OWNS_FRAME,
      },
    .ran = &deferred_ran,
    .destroyed = &deferred_destroyed,
  };
  CoroutineId deferred_id = rt_handle_alloc((GcPtr)deferred_coro);
  PromiseRef deferred_promise = rt_async_spawn_deferred(deferred_id);
  if (deferred_promise == (PromiseRef)0) {
    rt_thread_deinit();
    return 37;
  }
  if (deferred_ran != 0) {
    rt_thread_deinit();
    return 38;
  }
  if (deferred_promise != deferred_coro->header.promise) {
    rt_thread_deinit();
    return 39;
  }
  rt_drain_microtasks();
  if (deferred_ran != 1) {
    rt_thread_deinit();
    return 40;
  }
  if (deferred_destroyed != 1) {
    rt_thread_deinit();
    return 41;
  }
  if (rt_handle_load((HandleId)deferred_id) != (GcPtr)0) {
    rt_thread_deinit();
    return 42;
  }
  if (rt_promise_try_fulfill(deferred_promise)) {
    rt_thread_deinit();
    return 43;
  }
  rt_async_block_on(deferred_promise);

  // Cancellation: a deferred runtime-owned coroutine that never runs must still be destroyed and
  // have its CoroutineId handle freed (and its scheduled resume microtask discarded).
  int cancel_ran = 0;
  int cancel_destroyed = 0;
  NativeAsyncSmokeCoro* cancel_coro = (NativeAsyncSmokeCoro*)malloc(sizeof(NativeAsyncSmokeCoro));
  if (!cancel_coro) {
    rt_thread_deinit();
    return 44;
  }
  *cancel_coro = (NativeAsyncSmokeCoro){
    .header =
      {
        .vtable = &NATIVE_ASYNC_HEAP_VTABLE,
        .promise = (PromiseRef)0,
        .next_waiter = (Coroutine*)0,
        .flags = CORO_FLAG_RUNTIME_OWNS_FRAME,
      },
    .ran = &cancel_ran,
    .destroyed = &cancel_destroyed,
  };
  CoroutineId cancel_id = rt_handle_alloc((GcPtr)cancel_coro);
  PromiseRef cancel_promise = rt_async_spawn_deferred(cancel_id);
  if (cancel_promise == (PromiseRef)0) {
    rt_thread_deinit();
    return 45;
  }
  if (cancel_promise != cancel_coro->header.promise) {
    rt_thread_deinit();
    return 46;
  }
  if (cancel_ran != 0) {
    rt_thread_deinit();
    return 47;
  }
  if (cancel_destroyed != 0) {
    rt_thread_deinit();
    return 48;
  }
  rt_async_cancel_all();
  if (cancel_ran != 0) {
    rt_thread_deinit();
    return 49;
  }
  if (cancel_destroyed != 1) {
    rt_thread_deinit();
    return 50;
  }
  if (rt_handle_load((HandleId)cancel_id) != (GcPtr)0) {
    rt_thread_deinit();
    return 51;
  }
   // Draining after cancellation should be a no-op and must not run stale resume microtasks.
   rt_drain_microtasks();
   if (cancel_ran != 0) {
     rt_thread_deinit();
     return 52;
   }

   // Error reporting: exceeding async limits should populate `rt_async_take_last_error`.
   rt_async_set_limits(100000, 1);
   int runaway_ran = 0;
   Microtask runaway_task = {
     .func = set_int,
     .data = (uint8_t*)&runaway_ran,
   };
   rt_queue_microtask(runaway_task);
   // This enqueue should exceed `max_ready_queue_len=1` and record a last-error string.
   rt_queue_microtask(runaway_task);
   char* err = rt_async_take_last_error();
   if (err == (char*)0) {
     rt_thread_deinit();
     return 53;
   }
   if (strstr(err, "max_ready_queue_len=1") == (char*)0) {
     rt_async_free_c_string(err);
     rt_thread_deinit();
     return 54;
   }
   rt_async_free_c_string(err);
   // `take_last_error` clears the stored error.
   char* err2 = rt_async_take_last_error();
   if (err2 != (char*)0) {
     rt_async_free_c_string(err2);
     rt_thread_deinit();
     return 55;
   }
   // Clear the queued microtask so the runtime is idle at exit.
   rt_async_cancel_all();
   rt_async_set_limits(100000, 100000);

   rt_gc_safepoint();
   rt_gc_set_young_range((uint8_t*)0, (uint8_t*)0);
   rt_write_barrier_range((uint8_t*)0, (uint8_t*)0, 0);
   rt_thread_deinit();
   return 0;
}
"#,
  )
  .expect("write smoke.c");

  let include_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("include");
  // On Linux/ELF, `runtime-native` expects the final binary to export symbols
  // delimiting the (possibly empty) in-memory `.llvm_stackmaps` section
  // (`__start_llvm_stackmaps` / `__stop_llvm_stackmaps`, plus legacy aliases).
  //
  // When linking from C directly (bypassing Cargo/rustc), we must pass the same
  // linker-script fragment to the system linker.
  //
  // The correct fragment depends on which linker the system C toolchain drives:
  // - GNU ld: use `link/stackmaps_gnuld.ld` (inserting after `.text` can produce an RWX PT_LOAD).
  // - lld: use `link/stackmaps.ld` (lld keeps `.data.rel.ro.*` out of the executable segment).
  let stackmaps_ld = if cfg!(target_os = "linux") {
    let linker_version_out = Command::new(&cc.program)
      .args(&cc.args)
      .args(["-Wl,--version"])
      .output()
      .unwrap_or_else(|e| panic!("failed to query linker version via {}: {e}", cc.program));
    let linker_version = format!(
      "{}{}",
      String::from_utf8_lossy(&linker_version_out.stdout),
      String::from_utf8_lossy(&linker_version_out.stderr),
    );
    let cc_uses_lld = linker_version.to_ascii_lowercase().contains("lld");

    let lld_script = Path::new(env!("CARGO_MANIFEST_DIR"))
      .join("link")
      .join("stackmaps.ld");
    let gnuld_script = Path::new(env!("CARGO_MANIFEST_DIR"))
      .join("link")
      .join("stackmaps_gnuld.ld");
    let path = if cc_uses_lld { lld_script } else { gnuld_script };
    assert!(
      path.exists(),
      "missing stackmaps linker script at {}",
      path.display()
    );
    Some(path)
  } else {
    None
  };

  let mut cmd = Command::new(&cc.program);
  cmd
    .args(&cc.args)
    .arg("-std=c99")
    .arg("-I")
    .arg(&include_dir)
    .arg(&c_path)
    .args(stackmaps_ld.as_ref().map(|p| format!("-Wl,-T,{}", p.display())))
    .arg(&staticlib)
    .arg("-o")
    .arg(&bin_path);

  let compile = cmd.status().expect("compile + link smoke.c");

  assert!(
    compile.success(),
    "C compile/link failed with status: {compile:?}"
  );

  let run = Command::new(&bin_path)
    // Make the smoke binary deterministic and avoid flakiness from spawning a
    // large number of worker threads on first use (`rt_ensure_init`).
    .env("ECMA_RS_RUNTIME_NATIVE_THREADS", "1")
    .env("ECMA_RS_RUNTIME_NATIVE_BLOCKING_THREADS", "1")
    .status()
    .expect("run linked C binary");

  assert!(run.success(), "C binary exited non-zero: {run:?}");
}
