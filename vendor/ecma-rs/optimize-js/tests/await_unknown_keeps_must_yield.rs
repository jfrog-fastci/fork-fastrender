#![cfg(all(feature = "typed", feature = "semantic-ops"))]

use optimize_js::analysis::async_elision::AsyncElisionOptions;
use optimize_js::il::inst::{AwaitBehavior, Arg, Const, InstTyp};
use optimize_js::opt::optpass_async_elision::optpass_async_elision;
use optimize_js::{CompileCfgOptions, TopLevelMode};

fn is_internal_await(inst: &optimize_js::il::inst::Inst) -> bool {
  if inst.t != InstTyp::Call {
    return false;
  }
  let (_, callee, this, args, spreads) = inst.as_call();
  spreads.is_empty()
    && matches!(this, Arg::Const(Const::Undefined))
    && matches!(callee, Arg::Builtin(path) if path == "__optimize_js_await")
    && args.len() == 1
}

#[test]
fn await_unknown_keeps_must_yield() {
  let src = r#"
    declare function f(): Promise<number>;
    async function g() {
      return await f();
    }
  "#;

  let cfg_options = CompileCfgOptions {
    keep_ssa: true,
    run_opt_passes: false,
  };
  let mut program =
    optimize_js::compile_source_typed_cfg_options(src, TopLevelMode::Module, false, cfg_options)
      .expect("compile typed source");

  let options = AsyncElisionOptions {
    aggressive: true,
    rewrite: false,
  };
  optpass_async_elision(&mut program.top_level.body, options);
  for func in &mut program.functions {
    optpass_async_elision(&mut func.body, options);
  }

  let mut saw_await = false;
  for func in &program.functions {
    for label in func.body.graph.labels_sorted() {
      for inst in func.body.bblocks.get(label) {
        if !is_internal_await(inst) {
          continue;
        }
        saw_await = true;
        assert_eq!(
          inst.meta.await_behavior,
          Some(AwaitBehavior::MustYield),
          "expected await to remain MustYield"
        );
      }
    }
  }

  assert!(saw_await, "expected to find an await instruction in lowered IL");
}

