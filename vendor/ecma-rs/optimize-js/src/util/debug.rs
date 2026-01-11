use crate::cfg::cfg::Cfg;
use crate::il::inst::Inst;
use std::collections::BTreeMap;

#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "camelCase"))]
pub struct OptimizerDebugStep {
  pub name: String,
  pub bblock_order: Vec<u32>,
  pub bblocks: BTreeMap<u32, Vec<Inst>>,
  pub cfg_children: BTreeMap<u32, Vec<u32>>,
}

#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
pub struct OptimizerDebug {
  steps: Vec<OptimizerDebugStep>,
}

impl OptimizerDebug {
  pub fn new() -> Self {
    Self { steps: Vec::new() }
  }

  pub fn steps(&self) -> &[OptimizerDebugStep] {
    &self.steps
  }

  pub fn add_step(&mut self, name: impl AsRef<str>, cfg: &Cfg) {
    self.steps.push(OptimizerDebugStep {
      name: name.as_ref().to_string(),
      // We always recalculate as some steps may prune or add bblocks.
      bblock_order: cfg.graph.calculate_postorder(cfg.entry).0,
      bblocks: cfg.bblocks.all().map(|(k, v)| (k, v.clone())).collect(),
      cfg_children: cfg
        .graph
        .labels_sorted()
        .into_iter()
        .filter_map(|k| {
          let children = cfg.graph.children_sorted(k);
          (!children.is_empty()).then_some((k, children))
        })
        .collect(),
    });
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::cfg::cfg::{CfgBBlocks, CfgGraph};

  #[test]
  fn add_step_uses_cfg_entry_for_bblock_order() {
    let mut graph = CfgGraph::default();
    // Insert two disconnected nodes (0 and 1) so we can verify we start traversal from `cfg.entry`.
    graph.ensure_label(0);
    graph.ensure_label(1);

    let mut bblocks = CfgBBlocks::default();
    bblocks.add(0, vec![]);
    bblocks.add(1, vec![]);

    let cfg = Cfg {
      graph,
      bblocks,
      entry: 1,
    };

    let mut debug = OptimizerDebug::new();
    debug.add_step("test", &cfg);
    let step = debug.steps().last().unwrap();

    assert_eq!(step.bblock_order, vec![1]);
    assert_eq!(step.bblock_order[0], 1);
    assert!(!step.bblock_order.contains(&0));
  }
}
