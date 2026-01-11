#[path = "common/mod.rs"]
mod common;

use common::compile_source;
use optimize_js::analysis::analyze_cfg;
use optimize_js::TopLevelMode;
#[cfg(feature = "serde")]
use serde_json::to_string;

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
