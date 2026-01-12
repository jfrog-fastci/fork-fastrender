use std::sync::{Arc, Barrier};
use std::thread;

mod common;

use typecheck_ts::lib_support::CompilerOptions;
use typecheck_ts::{FileKey, MemoryHost, Program};

#[test]
fn concurrent_read_queries_do_not_deadlock() {
  let mut options = CompilerOptions::default();
  options.no_default_lib = true;

  let mut host = MemoryHost::with_options(options);
  host.add_lib(common::core_globals_lib());

  let util_key = FileKey::new("util.ts");
  let main_key = FileKey::new("main.ts");

  let util_src = r#"
export function add(a: number, b: number): number {
  return a + b;
}

export const meaning: number = 41;
export function id<T>(value: T): T {
  return value;
}
"#;

  let main_src = r#"
import { add, meaning, id } from "./util";

export const total = add(meaning, 1);

export function compute(x: number) {
  return add(total, id(x));
}
"#;

  host.insert(util_key.clone(), util_src);
  host.insert(main_key.clone(), main_src);

  let program = Arc::new(Program::new(host, vec![main_key.clone()]));

  let diagnostics = program.check();
  assert!(
    diagnostics.is_empty(),
    "expected diagnostics to be empty, got {diagnostics:#?}"
  );

  let main_id = program.file_id(&main_key).expect("main file id");
  let util_id = program.file_id(&util_key).expect("util file id");

  // Resolve the `total` export for repeatable `type_of_def` reads.
  let exports = program.exports_of(main_id);
  let total_def = exports
    .get("total")
    .and_then(|entry| entry.def)
    .expect("`total` export def");
  let expected_total_ty = program.type_of_def(total_def);

  // Use the `meaning` argument in `add(meaning, 1)` to exercise `symbol_at` and `type_at`.
  let meaning_offset = main_src
    .find("meaning, 1")
    .expect("meaning call site")
    .try_into()
    .expect("offset fits u32");
  let expected_symbol = program
    .symbol_at(main_id, meaning_offset)
    .expect("symbol for `meaning`");
  let expected_type_at = program
    .type_at(main_id, meaning_offset)
    .expect("type_at for `meaning`");

  // Confirm the import file is reachable and provides expected exports.
  let util_exports = program.exports_of(util_id);
  assert!(util_exports.contains_key("add"));

  let reader_threads = std::thread::available_parallelism()
    .map(|n| n.get())
    .unwrap_or(4)
    .min(8);
  const READ_ITERS: usize = 50;
  const WRITE_ITERS: usize = 3;

  let barrier = Arc::new(Barrier::new(reader_threads + 1));
  let mut handles = Vec::with_capacity(reader_threads);

  for thread_idx in 0..reader_threads {
    let program = Arc::clone(&program);
    let barrier = Arc::clone(&barrier);
    handles.push(thread::spawn(move || {
      barrier.wait();
      for iter in 0..READ_ITERS {
        // Spread out the more expensive export map query to keep the test fast.
        if (iter + thread_idx) % 10 == 0 {
          let exports = program.exports_of(main_id);
          assert!(exports.contains_key("total"));
          let exports = program.exports_of(util_id);
          assert!(exports.contains_key("add"));
        }

        assert_eq!(program.type_of_def(total_def), expected_total_ty);
        assert_eq!(
          program.symbol_at(main_id, meaning_offset),
          Some(expected_symbol)
        );
        assert_eq!(
          program.type_at(main_id, meaning_offset),
          Some(expected_type_at)
        );
      }
    }));
  }

  let writer = {
    let program = Arc::clone(&program);
    let barrier = Arc::clone(&barrier);
    thread::spawn(move || {
      barrier.wait();
      for _ in 0..WRITE_ITERS {
        let diagnostics = program.check();
        assert!(
          diagnostics.is_empty(),
          "expected diagnostics to be empty, got {diagnostics:#?}"
        );
      }
    })
  };

  for handle in handles {
    handle.join().expect("reader thread");
  }
  writer.join().expect("writer thread");
}
