use std::sync::Arc;

use typecheck_ts::{FileKey, MemoryHost, Program};

mod common;

#[test]
fn contextual_typing_picks_matching_overload_by_parameter_count() {
  let mut host = MemoryHost::new();
  host.add_lib(common::core_globals_lib());
  let file = FileKey::new("entry.ts");
  host.insert(
    file.clone(),
    Arc::<str>::from(
      r#"
declare function takes(cb: (x: string) => void): void;
declare function takes(cb: (x: number, y: number) => void): void;

takes((x, y) => {
  const n: number = x;
});
"#
      .to_string(),
    ),
  );

  let program = Program::new(host, vec![file]);
  let diagnostics = program.check();
  assert!(
    diagnostics.is_empty(),
    "expected no diagnostics, got {diagnostics:?}"
  );
}

#[test]
fn contextual_typing_picks_string_overload_for_single_param_callback() {
  let mut host = MemoryHost::new();
  host.add_lib(common::core_globals_lib());
  let file = FileKey::new("entry.ts");
  host.insert(
    file.clone(),
    Arc::<str>::from(
      r#"
declare function takes(cb: (x: string) => void): void;
declare function takes(cb: (x: number, y: number) => void): void;

takes((x) => {
  const s: string = x;
});
"#
      .to_string(),
    ),
  );

  let program = Program::new(host, vec![file]);
  let diagnostics = program.check();
  assert!(
    diagnostics.is_empty(),
    "expected no diagnostics, got {diagnostics:?}"
  );
}

