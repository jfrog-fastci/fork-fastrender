use typecheck_ts::{
  lower_call_count, parse_call_count, reset_lower_call_count, reset_parse_call_count, FileKey,
  MemoryHost, Program,
};

#[test]
fn bundled_libs_are_prepared_once_per_process() {
  let entry = FileKey::new("file0.ts");
  let mut host = MemoryHost::new();
  host.insert(entry.clone(), "export const x = 1;");

  reset_parse_call_count();
  reset_lower_call_count();
  let program1 = Program::new(host.clone(), vec![entry.clone()]);
  let diagnostics1 = program1.check();
  let first_parse_calls = parse_call_count();
  let first_lower_calls = lower_call_count();
  assert!(
    first_parse_calls > 1,
    "expected bundled libs to be parsed on the first run (got {first_parse_calls})"
  );
  assert!(
    first_lower_calls > 1,
    "expected bundled libs to be lowered on the first run (got {first_lower_calls})"
  );

  reset_parse_call_count();
  reset_lower_call_count();
  let program2 = Program::new(host, vec![entry]);
  let diagnostics2 = program2.check();
  let second_parse_calls = parse_call_count();
  let second_lower_calls = lower_call_count();

  assert_eq!(diagnostics1, diagnostics2);
  assert_eq!(
    second_parse_calls, 1,
    "expected the second run to only parse the root file (got {second_parse_calls})"
  );
  assert_eq!(
    second_lower_calls, 1,
    "expected the second run to only lower the root file (got {second_lower_calls})"
  );
}

