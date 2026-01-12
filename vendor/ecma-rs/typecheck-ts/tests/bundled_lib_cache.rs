#![cfg(feature = "bundled-libs")]

use typecheck_ts::lib_support::{CompilerOptions, LibManager};
use typecheck_ts::{
  lower_call_count, parse_call_count, reset_lower_call_count, reset_parse_call_count, FileKey,
  MemoryHost, Program,
};

#[test]
fn bundled_libs_are_cached_across_programs() {
  reset_parse_call_count();
  reset_lower_call_count();

  let mut host = MemoryHost::new();
  let entry = FileKey::new("file0.ts");
  host.insert(entry.clone(), "export const value = 1;");

  let program = Program::new(host.clone(), vec![entry.clone()]);
  let _ = program.check();
  let parse_after_first = parse_call_count();
  let lower_after_first = lower_call_count();

  let program = Program::new(host, vec![entry]);
  let _ = program.check();
  let parse_after_second = parse_call_count();
  let lower_after_second = lower_call_count();

  let delta_parse = parse_after_second.saturating_sub(parse_after_first);
  let delta_lower = lower_after_second.saturating_sub(lower_after_first);

  let bundled_lib_count = LibManager::new()
    .bundled_libs(&CompilerOptions::default())
    .files
    .len();
  assert!(
    bundled_lib_count > 1,
    "expected bundled TypeScript libs to be enabled and non-empty"
  );

  // The second program still parses/lowers the entry source file, but should not
  // redo work for every bundled lib.
  assert!(
    delta_parse < bundled_lib_count,
    "expected bundled lib parsing to be cached across Program instances; \
     second run performed {delta_parse} parses for {bundled_lib_count} bundled libs (first run: {parse_after_first}, second run: {parse_after_second})"
  );
  assert!(
    delta_lower < bundled_lib_count,
    "expected bundled lib lowering to be cached across Program instances; \
     second run performed {delta_lower} lowers for {bundled_lib_count} bundled libs (first run: {lower_after_first}, second run: {lower_after_second})"
  );
}

