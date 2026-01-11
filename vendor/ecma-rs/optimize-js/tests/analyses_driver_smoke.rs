#[path = "common/mod.rs"]
mod common;

use common::compile_source;
use optimize_js::analysis::{analyze_cfg, annotate_program};
use optimize_js::il::inst::{InstTyp, StringEncoding};
use optimize_js::TopLevelMode;
#[cfg(feature = "serde")]
use serde_json::to_string;
#[cfg(feature = "typed")]
use optimize_js::analysis::analyze_cfg_typed;
#[cfg(feature = "typed")]
use optimize_js::types::TypeContext;

#[test]
fn analyses_driver_smoke_is_deterministic() {
  let source = r#"
    const out = (() => {
      let x = 1;
      let y = x + 2;
      if (y < 10) {
        y = y + 1;
      }
      let s = Math.random() > 0.5 ? "hello" : null;
      if (s == null) {
        return "fallback";
      }
      return s;
    })();
    void out;
  "#;

  let program = compile_source(source, TopLevelMode::Module, false);
  let cfg = &program.top_level.body;

  let first = analyze_cfg(cfg);
  let second = analyze_cfg(cfg);

  assert_eq!(first, second, "analysis results should be stable across invocations");

  #[cfg(feature = "serde")]
  {
    let first_json = to_string(&first).expect("serialize first analysis result");
    let second_json = to_string(&second).expect("serialize second analysis result");
    assert_eq!(
      first_json, second_json,
      "serialized analysis results should be deterministic across invocations"
    );
  }

  assert!(
    first.range.entry(cfg.entry).is_some(),
    "range analysis should contain an entry for the CFG entry block"
  );
  assert!(
    first.nullability.entry_state(cfg.entry).is_reachable(),
    "nullability analysis entry state should be reachable"
  );
  assert!(
    first.encoding.block_entry(cfg.entry).is_some(),
    "encoding analysis should contain an entry state for the CFG entry block"
  );
}

#[test]
fn annotate_program_populates_inst_meta() {
  let source = r#"
    let s = maybeNull();

    // Ensure we have a non-constant string whose encoding can still be proven as Utf8.
    // `typeof` always produces ASCII output, and concatenating with a non-ASCII literal
    // forces the result encoding to Utf8.
    let encoded = typeof s + "π";

    if (s == null) {
      console.log(encoded);
    } else {
      console.log(s);
    }
  "#;

  let mut program = compile_source(source, TopLevelMode::Module, false);
  let _analyses = annotate_program(&mut program);

  let mut saw_narrowing = false;
  let mut saw_utf8_encoding = false;
  for (_label, block) in program.top_level.body.bblocks.all() {
    for inst in block {
      saw_narrowing |= inst.t == InstTyp::CondGoto && inst.meta.nullability_narrowing.is_some();
      saw_utf8_encoding |= inst.meta.result_type.string_encoding == Some(StringEncoding::Utf8);
    }
  }

  assert!(
    saw_narrowing,
    "expected at least one CondGoto to record InstMeta.nullability_narrowing"
  );
  assert!(
    saw_utf8_encoding,
    "expected at least one instruction to record Utf8 in InstMeta.result_type.string_encoding"
  );
}

#[cfg(feature = "typed")]
#[test]
fn analyses_driver_smoke_typed_is_deterministic() {
  let source = r#"
    const out = (() => {
      let x = 1;
      let y = x + 2;
      if (y < 10) {
        y = y + 1;
      }
      return y;
    })();
    void out;
  "#;

  let program = common::compile_source_typed(source, TopLevelMode::Module, false);
  let cfg = &program.top_level.body;
  let types = TypeContext::default();

  let first = analyze_cfg_typed(cfg, &types);
  let second = analyze_cfg_typed(cfg, &types);

  assert_eq!(first, second, "typed analysis results should be stable across invocations");

  #[cfg(feature = "serde")]
  {
    let first_json = to_string(&first).expect("serialize first typed analysis result");
    let second_json = to_string(&second).expect("serialize second typed analysis result");
    assert_eq!(
      first_json, second_json,
      "serialized typed analysis results should be deterministic across invocations"
    );
  }
}
