#![cfg(all(feature = "typed", feature = "semantic-ops"))]

use optimize_js::analysis::async_elision::AsyncElisionOptions;
use optimize_js::analysis::async_elision::await_operand;
use optimize_js::il::inst::AwaitBehavior;
use optimize_js::opt::optpass_async_elision::optpass_async_elision;
use optimize_js::{CompileCfgOptions, TopLevelMode};

fn run_pass_on_program(program: &mut optimize_js::Program, options: AsyncElisionOptions) {
  optpass_async_elision(&mut program.top_level.body, options);
  for func in &mut program.functions {
    optpass_async_elision(&mut func.body, options);
  }
}

#[test]
fn await_literal_may_not_yield() {
  let src = r#"
    async function f() {
      await 1;
    }
  "#;

  let cfg_options = CompileCfgOptions {
    keep_ssa: true,
    run_opt_passes: false,
  };
  let mut program =
    optimize_js::compile_source_typed_cfg_options(src, TopLevelMode::Module, false, cfg_options)
      .expect("compile typed source");

  run_pass_on_program(
    &mut program,
    AsyncElisionOptions {
      aggressive: true,
      rewrite: false,
    },
  );

  let mut found = false;
  for func in &program.functions {
    for label in func.body.graph.labels_sorted() {
      for inst in func.body.bblocks.get(label) {
        if await_operand(inst).is_some()
          && inst.meta.await_behavior == Some(AwaitBehavior::MayNotYield)
        {
          found = true;
        }
      }
    }
  }

  assert!(found, "expected at least one await to be marked MayNotYield");
}
