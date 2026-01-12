use std::sync::{Arc, Barrier};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use typecheck_ts::lib_support::CompilerOptions;
use typecheck_ts::{FileKey, MemoryHost, Program};

#[test]
fn concurrent_read_queries_do_not_deadlock_with_check() {
  // Keep the program small enough for CI but large enough that `check()` and
  // query threads overlap in practice.
  const BODY_COUNT: usize = 32;
  const READERS: usize = 4;
  const ITERATIONS: usize = 64;

  let mut host = MemoryHost::with_options(CompilerOptions {
    no_default_lib: true,
    ..Default::default()
  });

  let file = FileKey::new("entry.ts");
  let mut source = String::new();
  source.push_str("export const seed: number = 0;\n");
  for idx in 0..BODY_COUNT {
    source.push_str(&format!(
      "export function f{idx}(value: number): number {{ let x = value + {idx}; return x * {idx}; }}\n"
    ));
  }
  host.insert(file.clone(), Arc::<str>::from(source.clone()));

  let program = Arc::new(Program::new(host, vec![file.clone()]));
  let file_id = program.file_id(&file).expect("file id");

  // Pick offsets that land on identifiers so `type_at` and `symbol_at` can both
  // observe something meaningful.
  let offsets: Vec<u32> = source
    .match_indices("value")
    .map(|(idx, _)| idx as u32)
    .collect();
  assert!(!offsets.is_empty(), "expected to find query offsets");

  // Warm-up: ensure analysis + type interning has run before we start the
  // concurrent phase, otherwise the first `check()` would legitimately hold a
  // write lock while interning types and starve reader threads.
  let _ = program.type_at(file_id, offsets[0]);

  let barrier = Arc::new(Barrier::new(READERS + 1));
  let query_iterations = Arc::new(AtomicUsize::new(0));
  let check_started = Arc::new(AtomicBool::new(false));
  let check_finished = Arc::new(AtomicBool::new(false));
  let mut handles = Vec::new();

  {
    let program = Arc::clone(&program);
    let barrier = Arc::clone(&barrier);
    let check_started = Arc::clone(&check_started);
    let check_finished = Arc::clone(&check_finished);
    handles.push(std::thread::spawn(move || {
      barrier.wait();
      check_started.store(true, Ordering::Relaxed);
      program.check();
      check_finished.store(true, Ordering::Relaxed);
    }));
  }

  for tid in 0..READERS {
    let program = Arc::clone(&program);
    let barrier = Arc::clone(&barrier);
    let offsets = offsets.clone();
    let query_iterations = Arc::clone(&query_iterations);
    handles.push(std::thread::spawn(move || {
      barrier.wait();
      for idx in 0..ITERATIONS {
        let offset = offsets[(idx + tid) % offsets.len()];
        let _ = program.type_at(file_id, offset);
        let _ = program.symbol_at(file_id, offset);
        query_iterations.fetch_add(1, Ordering::Relaxed);
      }
    }));
  }

  // Poll for completion so a deadlock turns into a deterministic failure rather
  // than hanging the entire suite.
  let deadline = Instant::now() + Duration::from_secs(30);
  while handles.iter().any(|handle| !handle.is_finished()) {
    if Instant::now() > deadline {
      panic!(
        "concurrent query stress test timed out; possible deadlock (check_started={}, check_finished={}, query_iterations={})",
        check_started.load(Ordering::Relaxed),
        check_finished.load(Ordering::Relaxed),
        query_iterations.load(Ordering::Relaxed),
      );
    }
    std::thread::sleep(Duration::from_millis(10));
  }

  for handle in handles {
    handle.join().expect("thread should not panic");
  }
}
