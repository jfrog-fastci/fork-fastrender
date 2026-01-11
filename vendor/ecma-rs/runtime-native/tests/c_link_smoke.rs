use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use runtime_native::test_util::TestRuntimeGuard;

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

fn cargo_bin() -> String {
  std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_string())
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

  let _rt = TestRuntimeGuard::new();

  let tmp = tempfile::tempdir().expect("create temp dir");
  let build_target_dir = tmp.path().join("cargo-target");

  // Avoid deadlocking on Cargo's target-dir lock: the outer `cargo test` process holds a lock on
  // its own target directory for the duration of test execution. We build the staticlib into a
  // separate temp target dir instead.
  let build = Command::new(cargo_bin())
    .current_dir(workspace_root())
    .env("CARGO_TARGET_DIR", &build_target_dir)
    .arg("build")
    .arg("-p")
    .arg("runtime-native")
    .arg("--release")
    .status()
    .expect("build runtime-native staticlib");

  assert!(build.success(), "cargo build failed: {build:?}");

  let staticlib = build_target_dir.join("release").join("libruntime_native.a");
  assert!(
    staticlib.exists(),
    "missing staticlib at {} after build",
    staticlib.display()
  );

  let c_path = tmp.path().join("smoke.c");
  let bin_path = tmp.path().join("smoke");

  fs::write(
    &c_path,
    r#"
#include "runtime_native.h"
#include <unistd.h>
static void set_int(uint8_t* data) {
  int* flag = (int*)data;
  *flag = 1;
}

static void blocking_task(uint8_t* data, LegacyPromiseRef promise) {
  (void)data;
  rt_promise_resolve_legacy(promise, (ValueRef)0);
}

static void par_for_body(size_t i, uint8_t* data) {
  uint32_t* out = (uint32_t*)data;
  out[i] = (uint32_t)(i * 3u + 1u);
}

int main(void) {
  rt_thread_init(0);

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
  int timer_fired = 0;
  TimerId t = rt_set_timeout(set_int, (uint8_t*)&timer_fired, 200);

  int settled = 0;
  LegacyPromiseRef p = rt_spawn_blocking(blocking_task, (uint8_t*)0);
  rt_promise_then_legacy(p, set_int, (uint8_t*)&settled);

  // `rt_async_poll_legacy` should block in epoll_wait due to the timer, but wake promptly when the
  // blocking worker settles the promise.
  for (int i = 0; i < 1000 && !settled && !timer_fired; i++) {
    rt_async_poll_legacy();
  }
  rt_clear_timer(t);
  if (!settled) {
    rt_thread_deinit();
    return 1;
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
    .status()
    .expect("run linked C binary");

  assert!(run.success(), "C binary exited non-zero: {run:?}");
}
