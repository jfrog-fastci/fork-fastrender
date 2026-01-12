use optimize_js::analysis::find_loops::find_loops;
use optimize_js::cfg::cfg::Cfg;
use optimize_js::dom::Dom;
use optimize_js::il::inst::{Arg, BinOp, InstTyp};
use optimize_js::{compile_source, compile_source_with_cfg_options, CompileCfgOptions, TopLevelMode};

fn find_invariant_add_block(cfg: &Cfg, left: u32, right: u32) -> u32 {
  let mut matches = Vec::new();
  for (label, block) in cfg.bblocks.all() {
    if block.iter().any(|inst| {
      if inst.t != InstTyp::Bin {
        return false;
      }
      let (_tgt, l, op, r) = inst.as_bin();
      op == BinOp::Add
        && matches!(l, Arg::Var(v) if *v == left)
        && matches!(r, Arg::Var(v) if *v == right)
    }) {
      matches.push(label);
    }
  }
  matches.sort_unstable();
  matches.dedup();
  match matches.as_slice() {
    [label] => *label,
    [] => panic!("missing `{left} + {right}` Bin(Add) in CFG"),
    _ => panic!("expected a single `{left} + {right}` Bin(Add), got labels={matches:?}"),
  }
}

fn canonical_loop_preheader(cfg: &Cfg, header: u32, loop_nodes: &ahash::HashSet<u32>) -> u32 {
  let mut candidates: Vec<u32> = cfg
    .graph
    .parents_sorted(header)
    .into_iter()
    .filter(|p| !loop_nodes.contains(p))
    .filter(|p| cfg.graph.children_sorted(*p) == vec![header])
    .collect();
  candidates.sort_unstable();
  candidates.dedup();
  match candidates.as_slice() {
    [preheader] => *preheader,
    [] => panic!("missing canonical loop preheader for header {header}"),
    _ => panic!("expected one canonical preheader for header {header}, got {candidates:?}"),
  }
}

fn licm_source() -> &'static str {
  r#"
    function f(a, b, n) {
      let i = 0;
      while (i < n) {
        let c = a + b;
        i = i + c;
      }
      return i;
    }
    f(1, 2, 3);
  "#
}

#[test]
fn licm_runs_when_enabled_via_compile_cfg_options() {
  let options = CompileCfgOptions {
    keep_ssa: true,
    run_opt_passes: true,
    enable_licm: true,
    ..CompileCfgOptions::default()
  };
  let program =
    compile_source_with_cfg_options(licm_source(), TopLevelMode::Module, false, options)
      .expect("compile");
  assert_eq!(program.functions.len(), 1, "expected exactly one nested function");
  let func = &program.functions[0];

  let cfg = &func.body;
  let a = func.params[0];
  let b = func.params[1];

  let add_label = find_invariant_add_block(cfg, a, b);

  let dom = Dom::calculate(cfg);
  let loops = find_loops(cfg, &dom);
  assert_eq!(
    loops.len(),
    1,
    "expected a single natural loop in the test function"
  );

  let header = loops.keys().copied().min().expect("missing loop header");
  let nodes = loops.get(&header).expect("missing loop nodes for header");
  let preheader = canonical_loop_preheader(cfg, header, nodes);

  assert!(
    !nodes.contains(&add_label),
    "expected LICM to hoist `a + b` out of the loop, but it remained in loop nodes; add_label={add_label}, header={header}, nodes={nodes:?}"
  );
  assert_eq!(
    add_label, preheader,
    "expected `a + b` to be hoisted into the canonical preheader"
  );
}

#[test]
fn licm_is_disabled_by_default() {
  let program = compile_source(licm_source(), TopLevelMode::Module, false).expect("compile");
  assert_eq!(program.functions.len(), 1, "expected exactly one nested function");
  let func = &program.functions[0];
  let cfg = func.cfg_ssa().expect("expected SSA cfg to be preserved");

  let a = func.params[0];
  let b = func.params[1];
  let add_label = find_invariant_add_block(cfg, a, b);

  let dom = Dom::calculate(cfg);
  let loops = find_loops(cfg, &dom);
  assert_eq!(
    loops.len(),
    1,
    "expected a single natural loop in the test function"
  );
  let header = loops.keys().copied().min().expect("missing loop header");
  let nodes = loops.get(&header).expect("missing loop nodes for header");

  assert!(
    nodes.contains(&add_label),
    "expected `a + b` to remain in the loop when LICM is disabled by default, but it was hoisted; add_label={add_label}, header={header}, nodes={nodes:?}"
  );
}
