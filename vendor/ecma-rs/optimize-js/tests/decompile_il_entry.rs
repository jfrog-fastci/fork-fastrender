use optimize_js::cfg::cfg::{Cfg, CfgBBlocks, CfgGraph};
use optimize_js::decompile::il::decompile_function;
use optimize_js::ProgramFunction;

#[test]
fn decompile_flat_il_supports_nonzero_entry_label() {
  let mut graph = CfgGraph::default();
  graph.connect(1, 2);

  let mut bblocks = CfgBBlocks::default();
  bblocks.add(1, vec![]);
  bblocks.add(2, vec![]);

  let func = ProgramFunction {
    debug: None,
    body: Cfg {
      graph,
      bblocks,
      entry: 1,
    },
    stats: Default::default(),
  };

  let stmts = decompile_function(&func).expect("decompile");
  assert!(stmts.is_empty());
}

