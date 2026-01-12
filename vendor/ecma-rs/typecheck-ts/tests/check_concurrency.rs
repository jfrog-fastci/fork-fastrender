use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};

use typecheck_ts::db::{set_parallel_tracker, ParallelTracker};
use typecheck_ts::lib_support::CompilerOptions;
use typecheck_ts::{FileKey, MemoryHost, Program};

#[test]
fn program_check_does_not_block_symbol_queries() {
  let mut host = MemoryHost::with_options(CompilerOptions {
    no_default_lib: true,
    ..Default::default()
  });
  let file = FileKey::new("entry.ts");
  let mut source = String::new();
  // Force at least one initializer body so we exceed the parallel-check threshold even when the
  // number of functions happens to line up with it.
  source.push_str("export const seed: number = 0;\n");
  for idx in 0..128 {
    source.push_str(&format!(
      "export function f{idx}(value: number): number {{ let x = value + {idx}; return x * {idx}; }}\n"
    ));
  }
  let offset = source
    .find("f0")
    .expect("generated source should contain f0");
  host.insert(file.clone(), Arc::<str>::from(source));

  let program = Arc::new(Program::new(host, vec![file.clone()]));
  let file_id = program.file_id(&file).expect("file id");

  let tracker = Arc::new(ParallelTracker::new());
  set_parallel_tracker(Some(Arc::clone(&tracker)));
  struct Reset;
  impl Drop for Reset {
    fn drop(&mut self) {
      set_parallel_tracker(None);
    }
  }
  let _reset = Reset;

  let pool = rayon::ThreadPoolBuilder::new()
    .num_threads(4)
    .build()
    .expect("build rayon pool");

  let check_program = Arc::clone(&program);
  let handle = std::thread::spawn(move || pool.install(|| check_program.check()));

  // Wait for the check to begin checking bodies (which triggers the parallel tracker) so we know
  // we're in the expensive phase that used to hold the `ProgramState` mutex.
  let started = Instant::now();
  while tracker.max_active() == 0 && !handle.is_finished() {
    if started.elapsed() > Duration::from_secs(10) {
      panic!(
        "timeout waiting for program.check() to start checking bodies (max_active = {})",
        tracker.max_active()
      );
    }
    std::thread::yield_now();
  }
  assert!(
    !handle.is_finished(),
    "program.check() finished before body checking started"
  );

  let (tx, rx) = mpsc::channel();
  let query_program = Arc::clone(&program);
  std::thread::spawn(move || {
    let symbol = query_program.symbol_at(file_id, offset as u32);
    let _ = tx.send(symbol);
  });

  let symbol = rx
    // Allow extra headroom for slower CI environments while still ensuring the
    // query completes before `check()` finishes.
    .recv_timeout(Duration::from_secs(5))
    .expect("symbol_at should return while check() is still running");

  assert!(
    symbol.is_some(),
    "expected symbol_at to resolve the function name during check()"
  );
  assert!(
    !handle.is_finished(),
    "symbol_at returned only after check() completed; program.check() still holds the state lock"
  );

  let _ = handle.join();

  assert!(
    tracker.max_active() > 1,
    "expected parallel body checking (max_active = {})",
    tracker.max_active()
  );
}
