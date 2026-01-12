#[path = "common/mod.rs"]
mod common;

use common::compile_source;
use optimize_js::cfg::cfg::{Cfg, CfgBBlocks, CfgGraph};
use optimize_js::decompile::il::decompile_function;
use optimize_js::graph::Graph;
use optimize_js::il::inst::{Arg, Const, Inst};
use optimize_js::{OptimizationStats, ProgramFunction, TopLevelMode};
use parse_js::ast::stmt::Stmt;
use parse_js::num::JsNumber;

#[test]
fn decompile_function_emits_return_stmt() {
  let src = r#"
    const make = () => {
      return 1;
    };
    make();
  "#;
  let program = compile_source(src, TopLevelMode::Module, false);
  let func = program.functions.get(0).expect("expected nested function");
  let stmts = decompile_function(func).expect("decompile");
  assert!(
    stmts.iter().any(|stmt| matches!(stmt.stx.as_ref(), Stmt::Return(_))),
    "expected a Return statement, got: {stmts:?}"
  );
}

#[test]
fn decompile_function_emits_throw_stmt() {
  let src = r#"
    const fail = () => {
      throw 1;
    };
    fail();
  "#;
  let program = compile_source(src, TopLevelMode::Module, false);
  let func = program.functions.get(0).expect("expected nested function");
  let stmts = decompile_function(func).expect("decompile");
  assert!(
    stmts.iter().any(|stmt| matches!(stmt.stx.as_ref(), Stmt::Throw(_))),
    "expected a Throw statement, got: {stmts:?}"
  );
}

fn manual_function_with_unreachable_block(inst: Inst) -> ProgramFunction {
  let mut graph = Graph::<u32>::new();
  graph.ensure_node(&0);
  graph.ensure_node(&1);

  let mut bblocks = CfgBBlocks::default();
  bblocks.add(0, vec![inst]);
  bblocks.add(
    1,
    vec![Inst::throw(Arg::Const(Const::Num(JsNumber(2.0))))],
  );

  ProgramFunction {
    debug: None,
    meta: Default::default(),
    body: Cfg {
      graph: CfgGraph::from_graph(graph),
      bblocks,
      entry: 0,
    },
    params: Vec::new(),
    ssa_body: None,
    stats: OptimizationStats::default(),
  }
}

#[test]
fn decompile_stops_after_return() {
  let func =
    manual_function_with_unreachable_block(Inst::ret(Some(Arg::Const(Const::Num(JsNumber(1.0))))));
  let stmts = decompile_function(&func).expect("decompile");
  assert_eq!(
    stmts.len(),
    1,
    "expected decompiler to ignore unreachable blocks, got: {stmts:?}"
  );
  assert!(
    stmts.iter().any(|stmt| matches!(stmt.stx.as_ref(), Stmt::Return(_))),
    "expected a Return statement, got: {stmts:?}"
  );
}

#[test]
fn decompile_stops_after_throw() {
  let func =
    manual_function_with_unreachable_block(Inst::throw(Arg::Const(Const::Num(JsNumber(1.0)))));
  let stmts = decompile_function(&func).expect("decompile");
  assert_eq!(
    stmts.len(),
    1,
    "expected decompiler to ignore unreachable blocks, got: {stmts:?}"
  );
  assert!(
    stmts.iter().any(|stmt| matches!(stmt.stx.as_ref(), Stmt::Throw(_))),
    "expected a Throw statement, got: {stmts:?}"
  );
}
