#[path = "common/mod.rs"]
mod common;

use common::compile_source;
use optimize_js::analysis::driver::analyze_program_parallel;
use optimize_js::TopLevelMode;

#[test]
fn analysis_driver_parallel_is_deterministic() {
  let source = r#"
    const out = (() => {
      const mk = (x) => {
        const obj = { x };
        const inner = (y) => {
          if (y == null) {
            return "fallback";
          }
          return typeof obj.x + "π";
        };
        return inner(x);
      };
      return mk({ a: 1 }?.a);
    })();
    sink(out);
  "#;

  let program = compile_source(source, TopLevelMode::Module, false);
  let first = analyze_program_parallel(&program);
  let second = analyze_program_parallel(&program);

  assert_eq!(
    first.effects_summary, second.effects_summary,
    "effects summary should be deterministic across invocations"
  );
  assert_eq!(
    first.purity, second.purity,
    "purity results should be deterministic across invocations"
  );
  assert_eq!(
    first.escape, second.escape,
    "escape results should be deterministic across invocations"
  );
  assert_eq!(
    first.ownership, second.ownership,
    "ownership results should be deterministic across invocations"
  );
  assert_eq!(
    first.range, second.range,
    "range results should be deterministic across invocations"
  );
  assert_eq!(
    first.nullability, second.nullability,
    "nullability results should be deterministic across invocations"
  );
  assert_eq!(
    first.encoding, second.encoding,
    "encoding results should be deterministic across invocations"
  );

  #[cfg(feature = "serde")]
  {
    let first_json = serde_json::to_string(&first).expect("serialize first snapshot");
    let second_json = serde_json::to_string(&second).expect("serialize second snapshot");

    assert_eq!(
      first_json, second_json,
      "serialized results should be deterministic across invocations"
    );
  }
}
