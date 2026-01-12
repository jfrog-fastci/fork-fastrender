#![cfg(all(feature = "typed", feature = "semantic-ops"))]

use optimize_js::analysis::async_elision::AsyncElisionOptions;
use optimize_js::analysis::async_elision::await_operand;
use optimize_js::il::inst::{Arg, Const, InstTyp};
use optimize_js::opt::optpass_async_elision::optpass_async_elision;
use optimize_js::{CompileCfgOptions, TopLevelMode};

fn cfg_has_promise_all(cfg: &optimize_js::cfg::cfg::Cfg) -> bool {
  for label in cfg.graph.labels_sorted() {
    for inst in cfg.bblocks.get(label) {
      #[cfg(feature = "native-async-ops")]
      if inst.t == InstTyp::PromiseAll {
        return true;
      }
      if inst.t != InstTyp::Call {
        continue;
      }
      let (_, callee_arg, this_arg, _, _) = inst.as_call();
      if matches!(callee_arg, Arg::Builtin(path) if path == "Promise.all")
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
    ..Default::default()
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
      !cfg_has_promise_all(&func.body),
      "expected Promise.all op to be eliminated for singleton Promise.all([p])"
    );
    assert!(
      func
        .body
        .graph
        .labels_sorted()
        .into_iter()
        .flat_map(|label| func.body.bblocks.get(label))
        .any(|inst| await_operand(inst).is_some()),
      "expected lowered CFG to still contain an await (awaiting the single promise)"
    );
  }
}
