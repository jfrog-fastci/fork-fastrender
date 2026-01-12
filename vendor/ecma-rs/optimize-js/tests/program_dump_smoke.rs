#![cfg(all(feature = "serde", feature = "typed"))]

use optimize_js::analysis::annotate_program;
use optimize_js::dump::{dump_program, DumpOptions, DUMP_VERSION};
use optimize_js::{compile_source_typed, TopLevelMode};

#[test]
fn program_dump_smoke_contains_expected_fields() {
  let mut program = compile_source_typed(
    r#"
      function add1(x: number): number {
        return x + 1;
      }

      const out = add1(41);
      // Keep a call so we get non-default InstMeta.effects.
      console.log(out);
    "#,
    TopLevelMode::Module,
    false,
  )
  .expect("compile typed source");

  annotate_program(&mut program);

  let dump = dump_program(
    &program,
    DumpOptions {
      include_symbols: true,
      include_analyses: true,
    },
  );

  let json = dump.to_json_value();
  assert_eq!(
    json.get("version").and_then(|v| v.as_u64()),
    Some(DUMP_VERSION as u64)
  );
  assert_eq!(
    json.get("sourceMode").and_then(|v| v.as_str()),
    Some("module")
  );

  let top_cfg = json
    .pointer("/topLevel/cfg")
    .expect("topLevel.cfg should exist");
  assert!(
    top_cfg.get("bblocks").is_some(),
    "expected topLevel.cfg.bblocks"
  );
  assert!(
    json.get("analyses").is_some(),
    "expected analyses to be present when include_analyses = true"
  );

  // Spot-check at least one instruction has a meta object with the expected keys.
  let bblocks = top_cfg
    .get("bblocks")
    .and_then(|v| v.as_object())
    .expect("bblocks should be an object");
  let first_block = bblocks
    .values()
    .next()
    .and_then(|v| v.as_array())
    .expect("expected at least one basic block");
  let first_inst = first_block
    .first()
    .expect("expected at least one instruction");
  let meta = first_inst
    .get("meta")
    .expect("expected InstDump.meta");
  assert!(
    meta.get("effects").is_some(),
    "expected InstDump.meta.effects"
  );
  assert!(
    meta.get("ownership").is_some(),
    "expected InstDump.meta.ownership"
  );
  assert!(
    meta.get("argUseModes").is_some(),
    "expected InstDump.meta.argUseModes"
  );
  assert!(
    meta.get("excludesNullish").is_some(),
    "expected InstDump.meta.excludesNullish"
  );
}

