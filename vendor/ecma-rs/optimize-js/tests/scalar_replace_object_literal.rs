use optimize_js::analysis::escape::EscapeState;
use optimize_js::cfg::cfg::{Cfg, CfgBBlocks, CfgGraph};
use optimize_js::il::inst::{Arg, BinOp, Const, Inst, InstTyp};
use optimize_js::opt::optpass_scalar_replace::optpass_scalar_replace;
use parse_js::num::JsNumber;

fn cfg_single_block(insts: Vec<Inst>) -> Cfg {
  let mut graph = CfgGraph::default();
  graph.ensure_label(0);
  let mut bblocks = CfgBBlocks::default();
  bblocks.add(0, insts);
  Cfg {
    graph,
    bblocks,
    entry: 0,
  }
}

fn count_object_allocs(cfg: &optimize_js::cfg::cfg::Cfg) -> usize {
  cfg
    .bblocks
    .all()
    .flat_map(|(_, block)| block.iter())
    .filter(|inst| inst.t == InstTyp::ObjectLit)
    .count()
}

fn any_prop_ops(cfg: &optimize_js::cfg::cfg::Cfg) -> bool {
  cfg
    .bblocks
    .all()
    .flat_map(|(_, block)| block.iter())
    .any(|inst| {
      inst.t == InstTyp::PropAssign || (inst.t == InstTyp::Bin && inst.bin_op == BinOp::GetProp)
    })
}

#[test]
fn scalar_replace_eliminates_object_literal_and_field_ops() {
  // Build a minimal SSA-style CFG with a single object literal allocation + property ops.
  //
  // We set `result_escape` manually so the scalar replacement pass is allowed to run.
  let mut obj_lit = Inst::object_lit(
    0,
    vec![
      Arg::Builtin("__optimize_js_object_prop".to_string()),
      Arg::Const(Const::Str("x".to_string())),
      Arg::Const(Const::Num(JsNumber(1.0))),
      Arg::Builtin("__optimize_js_object_prop".to_string()),
      Arg::Const(Const::Str("y".to_string())),
      Arg::Const(Const::Num(JsNumber(2.0))),
    ],
  );
  obj_lit.meta.result_escape = Some(EscapeState::NoEscape);
  let mut cfg = cfg_single_block(vec![
    obj_lit,
    Inst::prop_assign(
      Arg::Var(0),
      Arg::Const(Const::Str("y".to_string())),
      Arg::Const(Const::Num(JsNumber(3.0))),
    ),
    Inst::bin(1, Arg::Var(0), BinOp::GetProp, Arg::Const(Const::Str("x".to_string()))),
    Inst::bin(2, Arg::Var(0), BinOp::GetProp, Arg::Const(Const::Str("y".to_string()))),
    Inst::bin(3, Arg::Var(1), BinOp::Add, Arg::Var(2)),
    Inst::ret(Some(Arg::Var(3))),
  ]);

  let allocs_before = count_object_allocs(&cfg);
  let prop_ops_before = any_prop_ops(&cfg);

  if allocs_before == 0 && !prop_ops_before {
    // `optimize-js` may run scalar replacement during compilation as part of its SSA metadata
    // pipeline. In that case this test just asserts the pass is idempotent.
    let result = optpass_scalar_replace(&mut cfg);
    assert!(
      !result.changed,
      "expected scalar replacement to be a no-op when object/field ops are already eliminated"
    );
    return;
  }

  assert_eq!(
    allocs_before,
    1,
    "expected one object allocation before scalar replacement"
  );
  assert!(prop_ops_before, "expected property ops before scalar replacement");

  let result = optpass_scalar_replace(&mut cfg);
  assert!(result.changed, "expected scalar replacement to report changes");

  assert_eq!(
    count_object_allocs(&cfg),
    0,
    "expected object allocation to be removed after scalar replacement"
  );
  assert!(
    !any_prop_ops(&cfg),
    "expected all GetProp/PropAssign ops to be eliminated after scalar replacement"
  );
}
