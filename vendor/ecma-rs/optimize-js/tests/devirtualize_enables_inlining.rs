#![cfg(feature = "typed")]

use optimize_js::il::inst::{Arg, InstTyp};
use optimize_js::{compile_source_typed_cfg_options, CompileCfgOptions, InlineOptions, TopLevelMode};

#[test]
fn devirtualize_enables_inlining_of_object_method_calls() {
  let src = r#"
    function f(x: number): number {
      return x + 1;
    }
    const o = { f };
    o.f(1);
  "#;

  let options = CompileCfgOptions {
    enable_devirtualize: true,
    inline: InlineOptions {
      enabled: true,
      threshold: 32,
      max_depth: 8,
    },
    ..CompileCfgOptions::default()
  };

  let program =
    compile_source_typed_cfg_options(src, TopLevelMode::Module, false, options).expect("compile");

  assert_eq!(program.functions.len(), 1, "expected exactly one nested function");

  let cfg = program.top_level.analyzed_cfg();

  let has_call_to_f = cfg
    .bblocks
    .all()
    .flat_map(|(_, b)| b.iter())
    .any(|inst| inst.t == InstTyp::Call && matches!(inst.as_call().1, Arg::Fn(0)));
  assert!(
    !has_call_to_f,
    "expected call to f to be inlined after devirtualization, but a Call Arg::Fn(0) remained"
  );

  let has_method_call = cfg
    .bblocks
    .all()
    .flat_map(|(_, b)| b.iter())
    .any(|inst| inst.t == InstTyp::Call && matches!(inst.as_call().2, Arg::Var(_)));
  assert!(
    !has_method_call,
    "expected method-style call to be removed after inlining, but a Call with non-undefined this remained"
  );
}
