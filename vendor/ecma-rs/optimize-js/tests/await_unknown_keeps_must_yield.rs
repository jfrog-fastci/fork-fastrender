#![cfg(all(feature = "typed", feature = "semantic-ops"))]

use optimize_js::analysis::async_elision::AsyncElisionOptions;
use optimize_js::analysis::async_elision::await_operand;
use optimize_js::opt::optpass_async_elision::optpass_async_elision;
use optimize_js::{CompileCfgOptions, TopLevelMode};

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
    ..Default::default()
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
        if await_operand(inst).is_none() {
          continue;
        }
        saw_await = true;
        assert!(
          inst.meta.await_behavior.is_none(),
          "expected await to remain MustYield (await_behavior unset), got {:?}",
          inst.meta.await_behavior
        );
      }
    }
  }

  assert!(saw_await, "expected to find an await instruction in lowered IL");
}
