#![cfg(all(feature = "typed", feature = "semantic-ops"))]

use optimize_js::analysis::async_elision::AsyncElisionOptions;
use optimize_js::il::inst::{Arg, Const, InstTyp};
use optimize_js::opt::optpass_async_elision::optpass_async_elision;
use optimize_js::{CompileCfgOptions, TopLevelMode};

fn cfg_has_builtin_call(cfg: &optimize_js::cfg::cfg::Cfg, callee: &str) -> bool {
  for label in cfg.graph.labels_sorted() {
    for inst in cfg.bblocks.get(label) {
      if inst.t != InstTyp::Call {
        continue;
      }
      let (_, callee_arg, this_arg, _, _) = inst.as_call();
      if matches!(callee_arg, Arg::Builtin(path) if path == callee)
        && matches!(this_arg, Arg::Const(Const::Undefined))
      {
        return true;
      }
    }
  }
  false
}

#[test]
fn promise_all_singleton_simplified() {
  let src = r#"
    async function f(p: Promise<number>) {
      return await Promise.all([p]);
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
    aggressive: false,
    rewrite: true,
  };
  optpass_async_elision(&mut program.top_level.body, options);
  for func in &mut program.functions {
    optpass_async_elision(&mut func.body, options);
  }

  // Promise.all singleton should be simplified away under rewrite=true.
  for func in &program.functions {
    assert!(
      !cfg_has_builtin_call(&func.body, "Promise.all"),
      "expected Promise.all call to be eliminated for singleton Promise.all([p])"
    );
    assert!(
      cfg_has_builtin_call(&func.body, "__optimize_js_await"),
      "expected lowered CFG to still contain an await (awaiting the single promise)"
    );
  }
}

