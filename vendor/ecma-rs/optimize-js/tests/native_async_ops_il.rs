#[path = "common/mod.rs"]
mod common;

use common::compile_source;
use optimize_js::cfg::cfg::Cfg;
use optimize_js::il::inst::{Arg, Inst, InstTyp};
use optimize_js::TopLevelMode;

fn collect_insts(cfg: &Cfg) -> Vec<Inst> {
  let mut blocks: Vec<_> = cfg.bblocks.all().collect();
  blocks.sort_by_key(|(label, _)| *label);
  blocks
    .into_iter()
    .flat_map(|(_, block)| block.iter().cloned())
    .collect()
}

fn has_call_to(insts: &[Inst], callee: &str) -> bool {
  insts.iter().any(|inst| {
    if inst.t != InstTyp::Call {
      return false;
    }
    let (_, callee_arg, _, _, _) = inst.as_call();
    matches!(callee_arg, Arg::Builtin(path) if path == callee)
  })
}

#[cfg(all(feature = "semantic-ops", feature = "native-async-ops"))]
mod native_async_ops {
  use super::*;

  #[test]
  fn lowers_await_to_structured_inst() {
    let program = compile_source("await p;", TopLevelMode::Module, false);
    let insts = collect_insts(program.top_level.analyzed_cfg());

    assert!(
      insts.iter().any(|inst| inst.t == InstTyp::Await),
      "expected Await instruction but got {insts:#?}"
    );
    assert!(
      !has_call_to(&insts, "__optimize_js_await"),
      "did not expect internal await helper call when native-async-ops is enabled"
    );
  }

  #[test]
  fn lowers_promise_all_to_structured_inst() {
    let program = compile_source("Promise.all([p1, p2]);", TopLevelMode::Module, false);
    let insts = collect_insts(program.top_level.analyzed_cfg());

    assert!(
      insts.iter().any(|inst| inst.t == InstTyp::PromiseAll),
      "expected PromiseAll instruction but got {insts:#?}"
    );
    assert!(
      !has_call_to(&insts, "Promise.all"),
      "did not expect builtin Promise.all call when native-async-ops is enabled"
    );
    assert!(
      !has_call_to(&insts, "__optimize_js_array"),
      "did not expect intermediate array construction when native-async-ops is enabled"
    );
  }

  #[test]
  fn lowers_promise_race_to_structured_inst() {
    let program = compile_source("Promise.race([p]);", TopLevelMode::Module, false);
    let insts = collect_insts(program.top_level.analyzed_cfg());

    assert!(
      insts.iter().any(|inst| inst.t == InstTyp::PromiseRace),
      "expected PromiseRace instruction but got {insts:#?}"
    );
    assert!(
      !has_call_to(&insts, "Promise.race"),
      "did not expect builtin Promise.race call when native-async-ops is enabled"
    );
    assert!(
      !has_call_to(&insts, "__optimize_js_array"),
      "did not expect intermediate array construction when native-async-ops is enabled"
    );
  }
}

#[cfg(all(feature = "semantic-ops", not(feature = "native-async-ops")))]
mod legacy_lowering {
  use super::*;

  #[test]
  fn still_lowers_semantic_async_ops_via_builtin_calls() {
    let program = compile_source(
      "await p; Promise.all([p1, p2]); Promise.race([p]);",
      TopLevelMode::Module,
      false,
    );
    let insts = collect_insts(program.top_level.analyzed_cfg());

    assert!(
      has_call_to(&insts, "__optimize_js_await"),
      "expected internal await helper call when native-async-ops is disabled"
    );
    assert!(
      has_call_to(&insts, "__optimize_js_array"),
      "expected intermediate array construction when native-async-ops is disabled"
    );
    assert!(
      has_call_to(&insts, "Promise.all"),
      "expected builtin Promise.all call when native-async-ops is disabled"
    );
    assert!(
      has_call_to(&insts, "Promise.race"),
      "expected builtin Promise.race call when native-async-ops is disabled"
    );
  }
}

