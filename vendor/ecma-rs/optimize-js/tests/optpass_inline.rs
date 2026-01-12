use optimize_js::cfg::cfg::{Cfg, CfgBBlocks, CfgGraph};
use optimize_js::il::inst::{Arg, BinOp, Const, Inst, InstTyp};
use optimize_js::opt::optpass_inline::optpass_inline;
use optimize_js::{CompileCfgOptions, FileId, InlineOptions, OptimizationStats, Program, ProgramFunction, TopLevelMode};

fn collect_insts(cfg: &Cfg) -> Vec<Inst> {
  let mut blocks: Vec<_> = cfg.bblocks.all().collect();
  blocks.sort_by_key(|(label, _)| *label);
  let mut insts = Vec::new();
  for (_, block) in blocks.into_iter() {
    insts.extend(block.iter().cloned());
  }
  insts
}

fn find_fn_with_direct_internal_call(program: &optimize_js::Program) -> (usize, usize) {
  for (fn_id, func) in program.functions.iter().enumerate() {
    let cfg = func.ssa_body.as_ref().expect("expected SSA cfg");
    for inst in collect_insts(cfg) {
      if inst.t != InstTyp::Call {
        continue;
      }
      let (_, callee, _, _, _) = inst.as_call();
      if let Arg::Fn(callee_id) = callee {
        return (fn_id, *callee_id);
      }
    }
  }
  panic!("expected to find a direct internal call (Arg::Fn) in some function");
}

#[test]
fn inlines_simple_arithmetic_iife() {
  let source = r#"
    export function main() {
      return ((x) => x + 1)(41);
    }
  "#;
  let mut program = optimize_js::compile_source_with_cfg_options(
    source,
    TopLevelMode::Module,
    false,
    CompileCfgOptions {
      run_opt_passes: false,
      ..CompileCfgOptions::default()
    },
  )
  .expect("compile");

  let (caller, _) = find_fn_with_direct_internal_call(&program);
  {
    let cfg = program.functions[caller].ssa_body.as_ref().unwrap();
    assert!(
      collect_insts(cfg).iter().any(|i| i.t == InstTyp::Call),
      "expected a Call before inlining"
    );
  }

  let pass = optpass_inline(
    &mut program,
    InlineOptions {
      enabled: true,
      threshold: 100,
      ..InlineOptions::default()
    },
    false,
  );
  assert!(pass.changed);
  assert!(pass.cfg_changed);

  let cfg = program.functions[caller].ssa_body.as_ref().unwrap();
  let insts = collect_insts(cfg);
  assert!(
    insts.iter().all(|i| i.t != InstTyp::Call),
    "expected Call to be removed after inlining, got {insts:?}"
  );
  assert!(
    insts
      .iter()
      .any(|i| i.t == InstTyp::Bin && i.bin_op == BinOp::Add),
    "expected an Add binop from the inlined body, got {insts:?}"
  );
}

#[test]
fn inlines_branching_function_and_inserts_phi() {
  let source = r#"
    export function main(cond) {
      return ((x) => {
        if (cond) return x + 1;
        return x + 2;
      })(10);
    }
  "#;
  let mut program = optimize_js::compile_source_with_cfg_options(
    source,
    TopLevelMode::Module,
    false,
    CompileCfgOptions {
      run_opt_passes: false,
      ..CompileCfgOptions::default()
    },
  )
  .expect("compile");

  let (caller, _) = find_fn_with_direct_internal_call(&program);
  optpass_inline(
    &mut program,
    InlineOptions {
      enabled: true,
      threshold: 100,
      ..InlineOptions::default()
    },
    false,
  );

  let cfg = program.functions[caller].ssa_body.as_ref().unwrap();
  let insts = collect_insts(cfg);
  assert!(
    insts.iter().all(|i| i.t != InstTyp::Call),
    "expected Call to be removed after inlining, got {insts:?}"
  );
  assert!(
    insts
      .iter()
      .any(|i| i.t == InstTyp::Phi && i.labels.len() == 2),
    "expected a phi node joining two return sites, got {insts:?}"
  );
}

#[test]
fn does_not_inline_when_callee_too_large() {
  // Build a small program where `Fn1` is called once from `Fn0`, but `Fn1` is over the inlining
  // instruction threshold.
  let mut top_graph = CfgGraph::default();
  top_graph.ensure_label(0);
  let mut top_blocks = CfgBBlocks::default();
  top_blocks.add(0, vec![]);
  let top_cfg = Cfg {
    graph: top_graph,
    bblocks: top_blocks,
    entry: 0,
  };

  let mut caller_graph = CfgGraph::default();
  caller_graph.ensure_label(0);
  let mut caller_blocks = CfgBBlocks::default();
  caller_blocks.add(
    0,
    vec![
      Inst::call(0, Arg::Fn(1), Arg::Const(Const::Undefined), vec![], vec![]),
      Inst::ret(Some(Arg::Var(0))),
    ],
  );
  let caller_cfg = Cfg {
    graph: caller_graph,
    bblocks: caller_blocks,
    entry: 0,
  };

  let mut callee_graph = CfgGraph::default();
  callee_graph.ensure_label(0);
  let mut callee_blocks = CfgBBlocks::default();
  let mut callee_insts = Vec::new();
  for i in 0..20 {
    callee_insts.push(Inst::var_assign(i, Arg::Const(Const::Undefined)));
  }
  callee_insts.push(Inst::ret(None));
  callee_blocks.add(0, callee_insts);
  let callee_cfg = Cfg {
    graph: callee_graph,
    bblocks: callee_blocks,
    entry: 0,
  };

  let mut program = Program {
    source_file: FileId(0),
    source_len: 0,
    top_level_mode: TopLevelMode::Module,
    symbols: None,
    top_level: ProgramFunction {
      debug: None,
      meta: Default::default(),
      body: top_cfg.clone(),
      params: Vec::new(),
      ssa_body: Some(top_cfg),
      stats: OptimizationStats::default(),
    },
    functions: vec![
      ProgramFunction {
        debug: None,
        meta: Default::default(),
        body: caller_cfg.clone(),
        params: Vec::new(),
        ssa_body: Some(caller_cfg),
        stats: OptimizationStats::default(),
      },
      ProgramFunction {
        debug: None,
        meta: Default::default(),
        body: callee_cfg.clone(),
        params: Vec::new(),
        ssa_body: Some(callee_cfg),
        stats: OptimizationStats::default(),
      },
    ],
  };

  let pass = optpass_inline(
    &mut program,
    InlineOptions {
      enabled: true,
      threshold: 1,
      ..InlineOptions::default()
    },
    false,
  );
  assert!(!pass.changed);

  let cfg = program.functions[0].ssa_body.as_ref().unwrap();
  let insts = collect_insts(cfg);
  assert!(
    insts.iter().any(|i| i.t == InstTyp::Call),
    "expected Call to remain when callee is over size limit, got {insts:?}"
  );
}

#[test]
fn inlines_recursive_callees_once() {
  // Build:
  // - caller (fn 0): %0 = call Fn1(); return %0
  // - callee (fn 1): call Fn1(); return undefined
  //
  // The recursive call inside the callee must remain a Call instruction (to avoid unbounded
  // growth), but the outer callsite should still be eligible for inlining.
  let mut top_graph = CfgGraph::default();
  top_graph.ensure_label(0);
  let mut top_blocks = CfgBBlocks::default();
  top_blocks.add(0, vec![]);
  let top_cfg = Cfg {
    graph: top_graph,
    bblocks: top_blocks,
    entry: 0,
  };

  let mut caller_graph = CfgGraph::default();
  caller_graph.ensure_label(0);
  let mut caller_blocks = CfgBBlocks::default();
  caller_blocks.add(
    0,
    vec![
      Inst::call(0, Arg::Fn(1), Arg::Const(Const::Undefined), vec![], vec![]),
      Inst::ret(Some(Arg::Var(0))),
    ],
  );
  let caller_cfg = Cfg {
    graph: caller_graph,
    bblocks: caller_blocks,
    entry: 0,
  };

  let mut callee_graph = CfgGraph::default();
  callee_graph.ensure_label(0);
  let mut callee_blocks = CfgBBlocks::default();
  callee_blocks.add(
    0,
    vec![
      Inst::call(None, Arg::Fn(1), Arg::Const(Const::Undefined), vec![], vec![]),
      Inst::ret(None),
    ],
  );
  let callee_cfg = Cfg {
    graph: callee_graph,
    bblocks: callee_blocks,
    entry: 0,
  };

  let mut program = Program {
    source_file: FileId(0),
    source_len: 0,
    top_level_mode: TopLevelMode::Module,
    symbols: None,
    top_level: ProgramFunction {
      debug: None,
      meta: Default::default(),
      body: top_cfg.clone(),
      params: Vec::new(),
      ssa_body: Some(top_cfg),
      stats: OptimizationStats::default(),
    },
    functions: vec![
      ProgramFunction {
        debug: None,
        meta: Default::default(),
        body: caller_cfg.clone(),
        params: Vec::new(),
        ssa_body: Some(caller_cfg),
        stats: OptimizationStats::default(),
      },
      ProgramFunction {
        debug: None,
        meta: Default::default(),
        body: callee_cfg.clone(),
        params: Vec::new(),
        ssa_body: Some(callee_cfg),
        stats: OptimizationStats::default(),
      },
    ],
  };

  let pass = optpass_inline(
    &mut program,
    InlineOptions {
      enabled: true,
      threshold: 100,
      ..InlineOptions::default()
    },
    false,
  );
  assert!(pass.changed, "expected recursive callee to be inlined once");

  let cfg = program.functions[0].ssa_body.as_ref().unwrap();
  let insts = collect_insts(cfg);
  let calls: Vec<_> = insts.iter().filter(|i| i.t == InstTyp::Call).collect();
  assert_eq!(
    calls.len(),
    1,
    "expected the original call to be inlined but the recursive call to remain, got {insts:?}"
  );
  let (tgt, callee, _, _, _) = calls[0].as_call();
  assert!(
    tgt.is_none(),
    "expected the remaining call to be the recursive call (no tgt), got {insts:?}"
  );
  assert!(
    matches!(callee, Arg::Fn(1)),
    "expected the remaining call to target the recursive callee, got {insts:?}"
  );
  assert!(
    calls.iter().all(|call| call.tgts.is_empty()),
    "expected call result to become unused after inlining, got {insts:?}"
  );
}

#[cfg(feature = "typed")]
#[test]
fn preserves_native_layout_on_inlined_call_result() {
  let source = r#"
    export function main(): number {
      return ((x: number): number => x + 1)(41);
    }
  "#;

  let mut program = optimize_js::compile_source_typed_cfg_options(
    source,
    TopLevelMode::Module,
    false,
    CompileCfgOptions {
      run_opt_passes: false,
      ..CompileCfgOptions::default()
    },
  )
  .expect("compile typed");

  let (caller, _) = find_fn_with_direct_internal_call(&program);
  let (call_tgt, call_layout) = {
    let cfg = program.functions[caller].ssa_body.as_ref().unwrap();
    let call = collect_insts(cfg)
      .into_iter()
      .find(|i| i.t == InstTyp::Call)
      .expect("expected call inst");
    let tgt = call.tgts[0];
    let layout = call.meta.native_layout;
    assert!(
      layout.is_some(),
      "expected native_layout to be populated on call inst in typed mode"
    );
    (tgt, layout)
  };

  optpass_inline(
    &mut program,
    InlineOptions {
      enabled: true,
      threshold: 100,
      ..InlineOptions::default()
    },
    false,
  );

  let cfg = program.functions[caller].ssa_body.as_ref().unwrap();
  let insts = collect_insts(cfg);
  assert!(
    insts.iter().all(|i| i.t != InstTyp::Call),
    "expected Call to be removed after inlining, got {insts:?}"
  );
  let def = insts
    .iter()
    .find(|i| i.tgts.get(0).copied() == Some(call_tgt))
    .expect("expected call result var to still be defined");
  assert_eq!(
    def.meta.native_layout, call_layout,
    "expected native_layout to be preserved on the instruction defining the call result"
  );
}
