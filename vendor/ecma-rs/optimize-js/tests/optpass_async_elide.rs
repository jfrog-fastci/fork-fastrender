#![cfg(all(feature = "semantic-ops", feature = "native-async-ops"))]

#[path = "common/mod.rs"]
mod common;

use common::compile_source;
use optimize_js::cfg::cfg::Cfg;
use optimize_js::il::inst::{Inst, InstTyp};
use optimize_js::TopLevelMode;

fn collect_insts(cfg: &Cfg) -> Vec<Inst> {
  let mut blocks: Vec<_> = cfg.bblocks.all().collect();
  blocks.sort_by_key(|(label, _)| *label);
  blocks
    .into_iter()
    .flat_map(|(_, block)| block.iter().cloned())
    .collect()
}

fn collect_all_insts(program: &optimize_js::Program) -> Vec<Inst> {
  let mut insts = collect_insts(program.top_level.analyzed_cfg());
  for func in &program.functions {
    insts.extend(collect_insts(func.analyzed_cfg()));
  }
  insts
}

#[test]
fn elides_known_resolved_await_in_async_function() {
  let program = compile_source("async function f(){ await 1; return 2 }", TopLevelMode::Module, false);
  let insts = collect_all_insts(&program);

  assert!(
    !insts.iter().any(|inst| inst.t == InstTyp::Await),
    "expected all known-resolved awaits to be elided, but got:\n{insts:#?}"
  );
}

#[test]
fn keeps_unknown_await() {
  let program = compile_source("async function f(){ await fetch(); }", TopLevelMode::Module, false);
  let insts = collect_all_insts(&program);

  assert!(
    insts.iter().any(|inst| inst.t == InstTyp::Await),
    "expected unknown awaits to remain, but got:\n{insts:#?}"
  );
}

