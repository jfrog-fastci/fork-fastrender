#[path = "common/mod.rs"]
mod common;
use common::compile_source;
use optimize_js::cfg::cfg::Cfg;
use optimize_js::il::inst::{Inst, InstMeta};
use optimize_js::il::inst::InstTyp;
use optimize_js::TopLevelMode;

fn cfg_contains_phi(cfg: &optimize_js::cfg::cfg::Cfg) -> bool {
  cfg
    .bblocks
    .all()
    .any(|(_, insts)| insts.iter().any(|inst| inst.t == InstTyp::Phi))
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct CfgSnapshot {
  entry: u32,
  bblocks: Vec<(u32, Vec<(Inst, InstMeta)>)>,
  children: Vec<(u32, Vec<u32>)>,
}

fn snapshot_cfg(cfg: &Cfg) -> CfgSnapshot {
  let labels = cfg.graph.labels_sorted();
  let bblocks = labels
    .iter()
    .map(|&label| {
      (
        label,
        cfg
          .bblocks
          .get(label)
          .iter()
          .map(|inst| (inst.clone(), inst.meta.clone()))
          .collect(),
      )
    })
    .collect();
  let children = labels
    .iter()
    .map(|&label| (label, cfg.graph.children_sorted(label)))
    .collect();

  CfgSnapshot {
    entry: cfg.entry,
    bblocks,
    children,
  }
}

#[test]
fn preserves_ssa_cfg_with_phis_alongside_deconstructed_cfg() {
  let source = r#"
    (() => {
      let x = 0;
      if (cond) {
        side_effect();
        x = 1;
      } else {
        side_effect();
        x = 2;
      }
      sink(x);
    })();
  "#;

  let program = compile_source(source, TopLevelMode::Module, false);

  let mut found_phi = false;
  for func in std::iter::once(&program.top_level).chain(program.functions.iter()) {
    let ssa_cfg = func.ssa_body.as_ref().expect("ssa_body should be populated");
    found_phi |= cfg_contains_phi(ssa_cfg);

    assert!(
      !cfg_contains_phi(&func.body),
      "deconstructed CFG should not contain Phi nodes"
    );
  }

  assert!(
    found_phi,
    "expected at least one Phi node in the preserved SSA CFG"
  );
}

#[test]
fn ssa_body_is_deterministic_across_compiles() {
  let source = r#"
    function mk(cond) {
      let a;
      if (cond) {
        a = {};
      } else {
        a = {};
      }
      return a;
    }
    sink(mk(unknown_cond()));
  "#;

  let first = compile_source(source, TopLevelMode::Module, false);
  let second = compile_source(source, TopLevelMode::Module, false);

  let first_snaps: Vec<_> = std::iter::once(&first.top_level)
    .chain(first.functions.iter())
    .map(|func| snapshot_cfg(func.ssa_body.as_ref().expect("ssa_body should be populated")))
    .collect();
  let second_snaps: Vec<_> = std::iter::once(&second.top_level)
    .chain(second.functions.iter())
    .map(|func| snapshot_cfg(func.ssa_body.as_ref().expect("ssa_body should be populated")))
    .collect();

  assert_eq!(
    first_snaps, second_snaps,
    "expected SSA CFG + metadata snapshot to be deterministic across compilation runs"
  );
}
