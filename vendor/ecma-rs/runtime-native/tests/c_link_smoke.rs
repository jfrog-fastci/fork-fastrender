use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

fn find_c_compiler() -> Option<String> {
  // Prefer $CC when set (common in CI / cross toolchains).
  if let Ok(cc) = std::env::var("CC") {
    if !cc.trim().is_empty() {
      return Some(cc);
    }
  }

  // Ubuntu images usually provide `cc`. Fall back to clang/gcc when needed.
  for candidate in ["cc", "clang", "gcc"] {
    if Command::new(candidate)
      .arg("--version")
      .stdout(std::process::Stdio::null())
      .stderr(std::process::Stdio::null())
      .status()
      .is_ok()
    {
      return Some(candidate.to_string());
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
#include <unistd.h>
#include <time.h>

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

int main(void) {
  rt_thread_init(0);

  // Global root registration (word-sized slot).
  static size_t global_root = 0;
  rt_global_root_register(&global_root);
  rt_global_root_unregister(&global_root);

  static const RtShapeDescriptor kShapes[1] = {
    {
      .size = 16,
      .align = 16,
      .flags = 0,
      .ptr_offsets = (const uint32_t*)0,
      .ptr_offsets_len = 0,
      .reserved = 0,
    },
  };
  rt_register_shape_table(kShapes, 1);

  RtShapeId shape = (RtShapeId)1;
  uint8_t* pinned = rt_alloc_pinned(16, shape);
  (void)pinned;

  // Smoke test: resolve a promise from a blocking worker and run its continuation on the
  // event loop thread.
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
  // script to the linker.
  let stackmaps_ld = if cfg!(target_os = "linux") {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
      .join("link")
      .join("stackmaps.ld");
    assert!(
      path.exists(),
      "missing stackmaps linker script at {}",
      path.display()
    );
    Some(path)
  } else {
    None
  };

  let mut cmd = Command::new(cc);
  cmd
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
