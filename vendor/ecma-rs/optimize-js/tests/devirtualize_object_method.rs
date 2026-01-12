#![cfg(feature = "typed")]

use optimize_js::il::inst::{Arg, InstTyp};
use optimize_js::{compile_source_typed_cfg_options, CompileCfgOptions, TopLevelMode};

#[test]
fn devirtualize_object_method_rewrites_call_callee_to_arg_fn() {
  let src = r#"
    function f(x: number): number {
      return x + 1;
    }
    const o = { f };
    o.f(1);
  "#;

  let options = CompileCfgOptions {
    enable_devirtualize: true,
    ..CompileCfgOptions::default()
  };

  let program =
    compile_source_typed_cfg_options(src, TopLevelMode::Module, false, options).expect("compile");

  // f is function 0 (declarations are hoisted in source order).
  assert_eq!(program.functions.len(), 1, "expected exactly one nested function");

  let cfg = program.top_level.analyzed_cfg();

  let mut method_calls = Vec::new();
  for (_, block) in cfg.bblocks.all() {
    for inst in block {
      if inst.t != InstTyp::Call {
        continue;
      }
      let (_tgt, callee, this, _args, _spreads) = inst.as_call();
      if matches!(this, Arg::Var(_)) {
        method_calls.push(callee.clone());
      }
    }
  }

  assert_eq!(
    method_calls.len(),
    1,
    "expected exactly one method-style call in top-level cfg, got {method_calls:?}"
  );
  assert!(
    matches!(method_calls[0], Arg::Fn(0)),
    "expected method call callee to be devirtualized to Arg::Fn(0), got {:?}",
    method_calls[0]
  );
}
