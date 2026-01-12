#![cfg(feature = "semantic-ops")]

#[path = "common/mod.rs"]
mod common;

use common::compile_source;
use optimize_js::il::inst::{Arg, InstTyp};
use optimize_js::{Program, TopLevelMode};

fn count_array_prototype_calls(program: &Program) -> usize {
  let mut count = 0;
  for (_, bb) in program.top_level.body.bblocks.all() {
    for inst in bb.iter() {
      if inst.t != InstTyp::Call {
        continue;
      }
      let Arg::Builtin(path) = &inst.args[0] else {
        continue;
      };
      if matches!(
        path.as_str(),
        "Array.prototype.map"
          | "Array.prototype.filter"
          | "Array.prototype.reduce"
          | "Array.prototype.find"
          | "Array.prototype.every"
          | "Array.prototype.some"
      ) {
        count += 1;
      }
    }
  }
  count
}

#[cfg(all(feature = "semantic-ops", feature = "native-fusion"))]
fn count_array_chain_insts(program: &Program) -> usize {
  let mut count = 0;
  for (_, bb) in program.top_level.body.bblocks.all() {
    for inst in bb.iter() {
      if inst.t == InstTyp::ArrayChain {
        count += 1;
      }
    }
  }
  count
}

#[cfg(all(feature = "semantic-ops", feature = "native-fusion"))]
#[test]
fn lowers_array_chain_to_single_il_inst() {
  let src = r#"
    const xs = [1, 2, 3, 4];
    function f(x) { return x + 1; }
    function g(x) { return (x % 2) === 0; }
    function h(x) { return x * 3; }
    const out = xs.map(f).filter(g).map(h);
    console.log(out);
  "#;

  let program = compile_source(src, TopLevelMode::Module, false);
  assert_eq!(
    count_array_chain_insts(&program),
    1,
    "expected a single ArrayChain inst in top-level CFG"
  );
  assert_eq!(
    count_array_prototype_calls(&program),
    0,
    "expected no Array.prototype.{{map,filter,reduce,...}} calls when fusion is enabled"
  );
}

#[cfg(all(feature = "semantic-ops", feature = "native-fusion"))]
#[test]
fn lowers_map_reduce_chain_to_single_il_inst() {
  let src = r#"
    const xs = [1, 2, 3];
    function f(x) { return x + 1; }
    function r(acc, x) { return acc + x; }
    const out = xs.map(f).reduce(r, 0);
    console.log(out);
  "#;

  let program = compile_source(src, TopLevelMode::Module, false);
  assert_eq!(
    count_array_chain_insts(&program),
    1,
    "expected a single ArrayChain inst in top-level CFG"
  );
  assert_eq!(
    count_array_prototype_calls(&program),
    0,
    "expected no Array.prototype.{{map,filter,reduce,...}} calls when fusion is enabled"
  );
}

#[cfg(all(feature = "semantic-ops", not(feature = "native-fusion")))]
#[test]
fn lowers_array_chain_to_builtin_calls_when_fusion_is_disabled() {
  let src = r#"
    const xs = [1, 2, 3, 4];
    function f(x) { return x + 1; }
    function g(x) { return (x % 2) === 0; }
    function h(x) { return x * 3; }
    const out = xs.map(f).filter(g).map(h);
    console.log(out);
  "#;

  let program = compile_source(src, TopLevelMode::Module, false);
  assert_eq!(
    count_array_prototype_calls(&program),
    3,
    "expected map/filter/map to lower to three builtin calls when fusion is disabled"
  );
}
