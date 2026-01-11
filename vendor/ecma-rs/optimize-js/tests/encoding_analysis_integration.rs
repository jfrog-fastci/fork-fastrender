#[path = "common/mod.rs"]
mod common;

use common::compile_source;
use optimize_js::analysis::encoding::analyze_cfg_encoding;
use optimize_js::cfg::cfg::{Cfg, CfgBBlocks, CfgGraph};
use optimize_js::graph::Graph;
use optimize_js::il::inst::{Arg, Const, InstTyp, StringEncoding};
use optimize_js::util::debug::OptimizerDebugStep;
use optimize_js::TopLevelMode;

fn cfg_from_debug_step(step: &OptimizerDebugStep) -> Cfg {
  let mut graph = Graph::<u32>::new();
  for (&label, children) in step.cfg_children.iter() {
    graph.ensure_node(&label);
    for child in children {
      graph.connect(&label, child);
    }
  }
  for label in step.bblocks.keys() {
    graph.ensure_node(label);
  }

  let graph = CfgGraph::from_graph(graph);
  let mut bblocks = CfgBBlocks::default();
  for (&label, insts) in step.bblocks.iter() {
    bblocks.add(label, insts.clone());
  }
  Cfg {
    graph,
    bblocks,
    entry: 0,
  }
}

fn find_const_str_var_assign(cfg: &Cfg, value: &str) -> (u32, u32) {
  let mut matches = Vec::new();
  for (label, block) in cfg.bblocks.all() {
    for inst in block {
      if inst.t != InstTyp::VarAssign {
        continue;
      }
      let (tgt, arg) = inst.as_var_assign();
      if matches!(arg, Arg::Const(Const::Str(s)) if s == value) {
        matches.push((label, tgt));
      }
    }
  }
  matches.sort_unstable();
  match matches.as_slice() {
    [single] => *single,
    [] => panic!("missing `VarAssign` with Const::Str({value:?}) in CFG"),
    _ => panic!(
      "expected exactly one `VarAssign` defining {value:?}, found: {matches:?}"
    ),
  }
}

fn find_template_call(cfg: &Cfg, value: &str) -> (u32, u32) {
  let mut matches = Vec::new();
  for (label, block) in cfg.bblocks.all() {
    for inst in block {
      if inst.t != InstTyp::Call {
        continue;
      }
      let (tgt, callee, _this, args, spreads) = inst.as_call();
      let Some(tgt) = tgt else {
        continue;
      };
      if !spreads.is_empty() {
        continue;
      }
      if !matches!(callee, Arg::Builtin(path) if path == "__optimize_js_template") {
        continue;
      }
      if args.len() != 1 {
        continue;
      }
      if !matches!(&args[0], Arg::Const(Const::Str(s)) if s == value) {
        continue;
      }
      matches.push((label, tgt));
    }
  }
  matches.sort_unstable();
  match matches.as_slice() {
    [single] => *single,
    [] => panic!("missing __optimize_js_template({value:?}) call in CFG"),
    _ => panic!(
      "expected exactly one `Call` to __optimize_js_template({value:?}), found: {matches:?}"
    ),
  }
}

#[test]
fn encoding_analysis_recognizes_template_lowering() {
  let src = r#"
    let a = "hello";
    let b = `world`;
    let c = `hé`;
  "#;

  // Use the debug snapshots so we can assert over the real lowered CFG before
  // DVN/copy propagation and dead-code elimination strip unused bindings from the
  // final program body.
  let program = compile_source(src, TopLevelMode::Module, true);
  let dbg = program
    .top_level
    .debug
    .as_ref()
    .expect("debug output enabled");
  let step = dbg
    .steps()
    .iter()
    .find(|step| step.name == "ssa_rename_targets")
    .expect("ssa_rename_targets debug step should exist");
  let cfg = cfg_from_debug_step(step);

  let result = analyze_cfg_encoding(&cfg);

  let (lbl_hello, tgt_hello) = find_const_str_var_assign(&cfg, "hello");
  let (lbl_world, tgt_world) = find_template_call(&cfg, "world");
  let (lbl_he, tgt_he) = find_template_call(&cfg, "hé");

  assert_eq!(
    result.encoding_at_exit(lbl_hello, tgt_hello),
    StringEncoding::Ascii
  );
  assert_eq!(
    result.encoding_at_exit(lbl_world, tgt_world),
    StringEncoding::Ascii
  );
  assert_eq!(
    result.encoding_at_exit(lbl_he, tgt_he),
    StringEncoding::Latin1
  );
}
