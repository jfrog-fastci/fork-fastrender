#![cfg(feature = "typed")]

use optimize_js::il::inst::{Arg, InstTyp};
use optimize_js::{compile_source_typed_cfg_options, CompileCfgOptions, TopLevelMode};

fn method_calls_have_indirect_callee(cfg: &optimize_js::cfg::cfg::Cfg) -> bool {
  let mut saw_method_call = false;
  for (_, block) in cfg.bblocks.all() {
    for inst in block {
      if inst.t != InstTyp::Call {
        continue;
      }
      let (_tgt, callee, this, _args, _spreads) = inst.as_call();
      if !matches!(this, Arg::Var(_)) {
        continue;
      }
      saw_method_call = true;
      if matches!(callee, Arg::Fn(_)) {
        return false;
      }
    }
  }
  saw_method_call
}

#[test]
fn devirtualize_does_not_fire_when_property_is_overwritten() {
  let src = r#"
    function f(x: number): number {
      return x + 1;
    }

    function g(x: number): number {
      return x + 2;
    }

    const o = { f };
    o.f = g;
    o.f(1);
  "#;

  let options = CompileCfgOptions {
    enable_devirtualize: true,
    ..CompileCfgOptions::default()
  };

  let program =
    compile_source_typed_cfg_options(src, TopLevelMode::Module, false, options).expect("compile");

  let cfg = program.top_level.analyzed_cfg();

  assert!(
    method_calls_have_indirect_callee(cfg),
    "expected method call to remain indirect when property is overwritten"
  );
}

#[test]
fn devirtualize_does_not_fire_when_object_escapes() {
  let src = r#"
    function f(x: number): number {
      return x + 1;
    }

    function sink(_x: unknown): void {}

    const o = { f };
    sink(o);
    o.f(1);
  "#;

  let options = CompileCfgOptions {
    enable_devirtualize: true,
    ..CompileCfgOptions::default()
  };

  let program =
    compile_source_typed_cfg_options(src, TopLevelMode::Module, false, options).expect("compile");

  let cfg = program.top_level.analyzed_cfg();

  assert!(
    method_calls_have_indirect_callee(cfg),
    "expected method call to remain indirect when object escapes via a call argument"
  );
}
