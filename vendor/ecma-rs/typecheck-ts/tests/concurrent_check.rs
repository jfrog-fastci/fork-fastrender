use std::sync::mpsc;
use std::sync::Arc;
use std::time::{Duration, Instant};

mod common;

use typecheck_ts::db::{set_parallel_tracker, ParallelTracker};
use typecheck_ts::lib_support::CompilerOptions;
use typecheck_ts::{BodyId, FileKey, MemoryHost, Program};

#[test]
fn program_check_does_not_block_concurrent_check_body() {
  let mut host = MemoryHost::with_options(CompilerOptions {
    no_default_lib: true,
    ..Default::default()
  });
  host.add_lib(common::core_globals_lib());
  let file = FileKey::new("entry.ts");

  // Generate enough bodies to ensure `Program::check()` takes the parallel body-checking path.
  let mut source = String::new();
  source.push_str("export const seed: number = 0;\n");
  for idx in 0..256 {
    source.push_str(&format!(
      "export function f{idx}(value: number): number {{ let x = value + {idx}; return x * {idx}; }}\n"
    ));
  }
  host.insert(file.clone(), Arc::<str>::from(source));

  let program = Arc::new(Program::new(host, vec![file.clone()]));
  let file_id = program.file_id(&file).expect("file id");
  let body: BodyId = program.file_body(file_id).expect("top-level body");

  let tracker = Arc::new(ParallelTracker::new());
  set_parallel_tracker(Some(Arc::clone(&tracker)));
  struct Reset;
  impl Drop for Reset {
    fn drop(&mut self) {
      set_parallel_tracker(None);
    }
  }
  let _reset = Reset;

  #[derive(Debug)]
  enum Event {
    CheckDone(Vec<diagnostics::Diagnostic>),
    BodyDone(bool),
  }

  let (tx, rx) = mpsc::channel::<Event>();

  let check_program = Arc::clone(&program);
  let check_tx = tx.clone();
  let check_thread = std::thread::spawn(move || {
    let pool = rayon::ThreadPoolBuilder::new()
      .num_threads(2)
      .build()
      .expect("build rayon pool");
    let diags = pool.install(|| check_program.check());
    check_tx
      .send(Event::CheckDone(diags))
      .expect("send check done");
  });

  let body_program = Arc::clone(&program);
  let body_tx = tx.clone();
  let body_tracker = Arc::clone(&tracker);
  let body_thread = std::thread::spawn(move || {
    let started = Instant::now();
    while body_tracker.max_active() == 0 {
      if started.elapsed() > Duration::from_secs(10) {
        panic!("timed out waiting for program.check() to start checking bodies");
      }
      std::thread::yield_now();
    }

    let res = body_program.check_body(body);
    body_tx
      .send(Event::BodyDone(res.diagnostics().is_empty()))
      .expect("send body done");
  });

  let first = rx
    .recv_timeout(Duration::from_secs(30))
    .expect("receive first event");
  let second = rx
    .recv_timeout(Duration::from_secs(30))
    .expect("receive second event");

  match (&first, &second) {
    (Event::BodyDone(body_ok), Event::CheckDone(diags)) => {
      assert!(*body_ok, "expected concurrent check_body to succeed");
      assert!(
        diags.is_empty(),
        "unexpected program.check() diagnostics: {diags:?}"
      );
    }
    (Event::CheckDone(_), Event::BodyDone(_)) => {
      panic!(
        "program.check() completed before concurrent check_body; \
         this likely means the ProgramState mutex was held for the full body-check sweep"
      );
    }
    other => panic!("unexpected event ordering: {other:?}"),
  }

  check_thread.join().expect("join check thread");
  body_thread.join().expect("join body thread");
}
