#![cfg(feature = "typed")]

use optimize_js::analysis::{validate_layouts, LayoutValidationMode};
use optimize_js::cfg::cfg::{Cfg, CfgBBlocks, CfgGraph};
use optimize_js::il::inst::{Arg, Const, Inst};
use parse_js::num::JsNumber;

#[test]
fn layout_phi_mismatch_errors() {
  let store = types_ts_interned::TypeStore::new();
  let prim = store.primitive_ids();
  let number_layout = store.layout_of(prim.number);
  let string_layout = store.layout_of(prim.string);

  let mut graph = CfgGraph::default();
  graph.connect(0, 1);
  graph.connect(0, 2);
  graph.connect(1, 3);
  graph.connect(2, 3);

  let mut number_inst = Inst::var_assign(10, Arg::Const(Const::Num(JsNumber(1.0))));
  number_inst.meta.native_layout = Some(number_layout);

  let mut string_inst = Inst::var_assign(11, Arg::Const(Const::Str("x".to_string())));
  string_inst.meta.native_layout = Some(string_layout);

  let mut phi = Inst::phi_empty(12);
  phi.insert_phi(1, Arg::Var(10));
  phi.insert_phi(2, Arg::Var(11));

  let mut bblocks = CfgBBlocks::default();
  bblocks.add(0, Vec::new());
  bblocks.add(1, vec![number_inst]);
  bblocks.add(2, vec![string_inst]);
  bblocks.add(3, vec![phi]);

  let cfg = Cfg {
    graph,
    bblocks,
    entry: 0,
  };

  let diagnostics = validate_layouts(&cfg, LayoutValidationMode::Strict)
    .expect_err("expected phi layout mismatch to be rejected");
  assert!(
    diagnostics.iter().any(|d| d.code.as_str() == "OPT0101"),
    "expected OPT0101 diagnostic, got {diagnostics:?}"
  );
}
