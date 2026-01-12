#![cfg(feature = "semantic-ops")]

#[path = "common/mod.rs"]
mod common;

use common::compile_source;
use optimize_js::analysis::driver::annotate_program;
use optimize_js::il::inst::{Arg, Inst, InstTyp, ParallelPlan};
use optimize_js::TopLevelMode;
use optimize_js::Program;

fn find_inst<'a>(program: &'a Program, pred: impl Fn(&Inst) -> bool) -> Option<&'a Inst> {
  let cfgs = std::iter::once(program.top_level.analyzed_cfg())
    .chain(program.functions.iter().map(|f| f.analyzed_cfg()));
  for cfg in cfgs {
    for label in cfg.graph.labels_sorted() {
      for inst in cfg.bblocks.get(label).iter() {
        if pred(inst) {
          return Some(inst);
        }
      }
    }
  }
  None
}

#[cfg(feature = "native-async-ops")]
#[test]
fn promise_all_of_two_pure_calls_is_parallelizable() {
  let source = r#"
    async function test() {
      const a = () => 1;
      const b = () => 2;
      return await Promise.all([a(), b()]);
    }
    void test();
  "#;

  let mut program = compile_source(source, TopLevelMode::Module, false);
  let _analyses = annotate_program(&mut program);

  let inst = find_inst(&program, |inst| inst.t == InstTyp::PromiseAll)
    .expect("expected PromiseAll instruction");
  assert!(
    matches!(inst.meta.parallel, Some(ParallelPlan::SpawnAll)),
    "expected PromiseAll to be parallelizable but got {:?}",
    inst.meta.parallel
  );
}

#[cfg(feature = "native-async-ops")]
#[test]
fn promise_all_with_heap_write_is_not_parallelizable() {
  let source = r#"
    async function test(obj) {
      const a = () => { obj.x = 1; return 1; };
      const b = () => 2;
      return await Promise.all([a(), b()]);
    }
    void test({});
  "#;

  let mut program = compile_source(source, TopLevelMode::Module, false);
  let _analyses = annotate_program(&mut program);

  let inst = find_inst(&program, |inst| inst.t == InstTyp::PromiseAll)
    .expect("expected PromiseAll instruction");
  assert!(
    matches!(inst.meta.parallel, Some(ParallelPlan::NotParallelizable(_))),
    "expected PromiseAll to be non-parallelizable but got {:?}",
    inst.meta.parallel
  );
}

#[test]
fn array_map_with_pure_callback_is_parallelizable() {
  let source = r#"
    function run(arr) {
      return arr.map(x => x + 1);
    }
    void run([1, 2, 3]);
  "#;

  let mut program = compile_source(source, TopLevelMode::Module, false);
  let _analyses = annotate_program(&mut program);

  let inst = find_inst(&program, |inst| {
    inst.t == InstTyp::Call
      && matches!(inst.args.get(0), Some(Arg::Builtin(path)) if path == "Array.prototype.map")
  })
  .expect("expected Array.prototype.map call instruction");

  assert!(
    matches!(inst.meta.parallel, Some(ParallelPlan::Parallelizable)),
    "expected Array.prototype.map to be parallelizable but got {:?}",
    inst.meta.parallel
  );
}

#[test]
fn array_map_with_impure_callback_is_not_parallelizable() {
  let source = r#"
    function run(arr) {
      const obj = {};
      return arr.map(x => { obj.x = x; return x; });
    }
    void run([1, 2, 3]);
  "#;

  let mut program = compile_source(source, TopLevelMode::Module, false);
  let _analyses = annotate_program(&mut program);

  let inst = find_inst(&program, |inst| {
    inst.t == InstTyp::Call
      && matches!(inst.args.get(0), Some(Arg::Builtin(path)) if path == "Array.prototype.map")
  })
  .expect("expected Array.prototype.map call instruction");

  assert!(
    matches!(inst.meta.parallel, Some(ParallelPlan::NotParallelizable(_))),
    "expected Array.prototype.map to be non-parallelizable but got {:?}",
    inst.meta.parallel
  );
}

#[cfg(feature = "native-fusion")]
#[test]
fn array_chain_map_filter_pure_is_parallelizable() {
  let source = r#"
    function run(arr) {
      return arr.map(x => x + 1).filter(x => x > 0);
    }
    void run([1, 2, 3]);
  "#;

  let mut program = compile_source(source, TopLevelMode::Module, false);
  let _analyses = annotate_program(&mut program);

  let inst = find_inst(&program, |inst| inst.t == InstTyp::ArrayChain)
    .expect("expected ArrayChain instruction");
  assert!(
    matches!(inst.meta.parallel, Some(ParallelPlan::Parallelizable)),
    "expected ArrayChain to be parallelizable but got {:?}",
    inst.meta.parallel
  );
}
