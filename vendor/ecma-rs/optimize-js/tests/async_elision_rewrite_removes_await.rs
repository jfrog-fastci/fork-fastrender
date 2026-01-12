#![cfg(all(feature = "typed", feature = "semantic-ops"))]

use optimize_js::analysis::async_elision::AsyncElisionOptions;
use optimize_js::analysis::async_elision::await_operand;
use optimize_js::opt::optpass_async_elision::optpass_async_elision;
use optimize_js::{CompileCfgOptions, TopLevelMode};

fn has_await(cfg: &optimize_js::cfg::cfg::Cfg) -> bool {
  for label in cfg.graph.labels_sorted() {
    for inst in cfg.bblocks.get(label) {
      if await_operand(inst).is_some() {
        return true;
      }
    }
  }
  false
}

#[test]
fn async_elision_rewrite_removes_await() {
  let src = r#"
    async function f() {
      return await 1;
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
    rewrite: true,
  };
  optpass_async_elision(&mut program.top_level.body, options);
  for func in &mut program.functions {
    optpass_async_elision(&mut func.body, options);
  }

  assert!(
    program.functions.iter().all(|f| !has_await(&f.body)),
    "expected await to be rewritten away when rewrite=true"
  );
}
