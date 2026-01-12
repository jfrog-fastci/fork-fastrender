use optimize_js::analysis::find_loops::find_loops;
use optimize_js::cfg::cfg::Cfg;
use optimize_js::dom::Dom;
use optimize_js::il::inst::{Arg, BinOp, InstTyp};
use optimize_js::{compile_source_with_cfg_options, CompileCfgOptions, TopLevelMode};
use std::collections::HashMap;
use std::collections::HashSet;

fn compile_ssa_cfg(source: &str, enable_licm: bool) -> Cfg {
  let options = CompileCfgOptions {
    enable_licm,
    ..CompileCfgOptions::default()
  };
  let program = compile_source_with_cfg_options(source, TopLevelMode::Module, false, options)
    .expect("compile source");
  program
    .top_level
    .ssa_body
    .expect("expected SSA CFG to be preserved in ProgramFunction::ssa_body")
}

fn build_def_blocks(cfg: &Cfg) -> HashMap<u32, u32> {
  let mut defs = HashMap::<u32, u32>::new();
  let mut labels: Vec<u32> = cfg.bblocks.all().map(|(l, _)| l).collect();
  labels.sort_unstable();
  for label in labels {
    for inst in cfg.bblocks.get(label) {
      for &tgt in &inst.tgts {
        defs.insert(tgt, label);
      }
    }
  }
  defs
}

fn find_single_loop(cfg: &Cfg) -> (u32, HashSet<u32>) {
  let dom = Dom::calculate(cfg);
  let loops = find_loops(cfg, &dom);
  assert_eq!(
    loops.len(),
    1,
    "expected test program to produce exactly one loop, got {loops:?}"
  );
  let (&header, nodes) = loops.iter().next().expect("missing loop");
  (header, nodes.iter().copied().collect())
}

fn count_loop_invariant_adds(cfg: &Cfg, nodes: &HashSet<u32>) -> usize {
  let def_blocks = build_def_blocks(cfg);
  let mut labels: Vec<u32> = nodes.iter().copied().collect();
  labels.sort_unstable();
  let mut count = 0usize;

  for label in labels {
    for inst in cfg.bblocks.get(label).iter() {
      if inst.t != InstTyp::Bin || inst.bin_op != BinOp::Add {
        continue;
      }

      let args_outside_loop = inst.args.iter().all(|arg| match arg {
        Arg::Var(v) => {
          let def_block = def_blocks.get(v).copied().unwrap_or(cfg.entry);
          !nodes.contains(&def_block)
        }
        _ => true,
      });

      if args_outside_loop {
        count += 1;
      }
    }
  }

  count
}

fn count_preheader_invariant_adds(cfg: &Cfg, preheader: u32, loop_nodes: &HashSet<u32>) -> usize {
  let def_blocks = build_def_blocks(cfg);
  cfg
    .bblocks
    .get(preheader)
    .iter()
    .filter(|inst| inst.t == InstTyp::Bin && inst.bin_op == BinOp::Add)
    .filter(|inst| {
      inst.args.iter().all(|arg| match arg {
        Arg::Var(v) => {
          let def_block = def_blocks.get(v).copied().unwrap_or(cfg.entry);
          !loop_nodes.contains(&def_block)
        }
        _ => true,
      })
    })
    .count()
}

#[test]
fn licm_flag_enabled_hoists_invariant_arithmetic() {
  let source = r#"
    let a = unknown_a();
    let b = unknown_b();
    let i = 0;
    let sum = 0;

    if (unknown_cond()) {
      while (i < 10) {
        let c = a + b;    // loop-invariant
        sum = sum + c;
        i = i + 1;
      }
    }

    sink(sum);
  "#;

  let cfg = compile_ssa_cfg(source, true);
  let (header, loop_nodes) = find_single_loop(&cfg);

  let outside_preds: Vec<u32> = cfg
    .graph
    .parents_sorted(header)
    .into_iter()
    .filter(|p| !loop_nodes.contains(p))
    .collect();
  assert_eq!(
    outside_preds.len(),
    1,
    "expected structured loop to have exactly one outside predecessor, got {outside_preds:?}"
  );
  let preheader = outside_preds[0];
  assert_eq!(
    cfg.graph.children_sorted(preheader),
    vec![header],
    "expected LICM to ensure a canonical preheader (single predecessor with only one successor)"
  );

  assert_eq!(
    count_loop_invariant_adds(&cfg, &loop_nodes),
    0,
    "expected LICM to hoist loop-invariant add out of the loop"
  );
  assert!(
    count_preheader_invariant_adds(&cfg, preheader, &loop_nodes) > 0,
    "expected preheader block to contain the hoisted loop-invariant add"
  );
}

#[test]
fn licm_flag_disabled_does_not_hoist_invariant_arithmetic() {
  let source = r#"
    let a = unknown_a();
    let b = unknown_b();
    let i = 0;
    let sum = 0;

    if (unknown_cond()) {
      while (i < 10) {
        let c = a + b;    // loop-invariant
        sum = sum + c;
        i = i + 1;
      }
    }

    sink(sum);
  "#;

  let cfg = compile_ssa_cfg(source, false);
  let (header, loop_nodes) = find_single_loop(&cfg);

  let outside_preds: Vec<u32> = cfg
    .graph
    .parents_sorted(header)
    .into_iter()
    .filter(|p| !loop_nodes.contains(p))
    .collect();
  assert_eq!(outside_preds.len(), 1, "expected structured loop with one outside predecessor");
  let outside_pred = outside_preds[0];
  assert!(
    cfg.graph.children_sorted(outside_pred).len() > 1,
    "expected loop entry block to have multiple successors (no canonical preheader) when LICM is disabled"
  );

  assert_eq!(
    count_loop_invariant_adds(&cfg, &loop_nodes),
    1,
    "expected loop-invariant add to remain in the loop when LICM is disabled"
  );
}

