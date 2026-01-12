#![cfg(feature = "typed")]

use optimize_js::analysis::{validate_layouts, LayoutValidationMode};
use optimize_js::cfg::cfg::{Cfg, CfgBBlocks, CfgGraph};
use optimize_js::il::inst::{Arg, Const, Inst};
use parse_js::num::JsNumber;

#[test]
fn layout_copy_mismatch_errors() {
  let store = types_ts_interned::TypeStore::new();
  let prim = store.primitive_ids();
  let number_layout = store.layout_of(prim.number);
  let string_layout = store.layout_of(prim.string);

  let mut graph = CfgGraph::default();
  graph.ensure_label(0);

  let mut src = Inst::var_assign(10, Arg::Const(Const::Num(JsNumber(1.0))));
  src.meta.native_layout = Some(number_layout);

  let mut copy = Inst::var_assign(11, Arg::Var(10));
  // Deliberately lie about the target layout to ensure the verifier rejects the copy.
  copy.meta.native_layout = Some(string_layout);

  let mut bblocks = CfgBBlocks::default();
  bblocks.add(0, vec![src, copy]);

  let cfg = Cfg {
    graph,
    bblocks,
    entry: 0,
  };

  let diagnostics = validate_layouts(&cfg, LayoutValidationMode::Strict)
    .expect_err("expected copy layout mismatch to be rejected");
  assert!(
    diagnostics.iter().any(|d| d.code.as_str() == "OPT0102"),
    "expected OPT0102 diagnostic, got {diagnostics:?}"
  );
}
