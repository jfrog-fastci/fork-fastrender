use optimize_js::analysis::{escape, interproc_escape};
use optimize_js::analysis::escape::EscapeState;
use optimize_js::cfg::cfg::{Cfg, CfgBBlocks, CfgGraph};
use optimize_js::il::inst::{Arg, Const, Inst};
use optimize_js::{OptimizationStats, Program, ProgramFunction, TopLevelMode};

const EXIT: u32 = u32::MAX;

fn cfg_single_block(insts: Vec<Inst>) -> Cfg {
  let mut graph = CfgGraph::default();
  graph.connect(0, EXIT);
  let mut bblocks = CfgBBlocks::default();
  bblocks.add(0, insts);
  bblocks.add(EXIT, Vec::new());
  Cfg {
    graph,
    bblocks,
    entry: 0,
  }
}

fn func(cfg: Cfg, params: Vec<u32>) -> ProgramFunction {
  ProgramFunction {
    debug: None,
    meta: Default::default(),
    body: cfg,
    params,
    ssa_body: None,
    stats: OptimizationStats::default(),
  }
}

fn escape_of(result: &escape::EscapeResult, var: u32) -> EscapeState {
  result.get(&var).copied().unwrap_or(EscapeState::NoEscape)
}

#[test]
fn passing_alloc_to_helper_returning_param_does_not_force_global_escape() {
  // helper(x) { return x; }
  let helper = func(cfg_single_block(vec![Inst::ret(Some(Arg::Var(0)))]), vec![0]);

  // caller() { const o = {}; helper(o); }
  let caller = func(
    cfg_single_block(vec![
      Inst::call(
        0,
        Arg::Builtin("__optimize_js_object".to_string()),
        Arg::Const(Const::Undefined),
        Vec::new(),
        Vec::new(),
      ),
      Inst::call(
        None::<u32>,
        Arg::Fn(0),
        Arg::Const(Const::Undefined),
        vec![Arg::Var(0)],
        Vec::new(),
      ),
      Inst::ret(None),
    ]),
    Vec::new(),
  );

  let program = Program {
    source_file: optimize_js::FileId(0),
    source_len: 0,
    functions: vec![helper, caller],
    top_level: func(cfg_single_block(Vec::new()), Vec::new()),
    top_level_mode: TopLevelMode::Module,
    symbols: None,
  };

  let summaries = interproc_escape::compute_program_escape_summaries(&program);
  let caller_escape = escape::analyze_cfg_escapes_with_params_and_summaries(
    &program.functions[1].body,
    &program.functions[1].params,
    Some(&summaries),
    None,
  );
  assert_eq!(escape_of(&caller_escape, 0), EscapeState::NoEscape);
}

#[test]
fn returning_helper_call_result_marks_alloc_as_return_escape() {
  // helper(x) { return x; }
  let helper = func(cfg_single_block(vec![Inst::ret(Some(Arg::Var(0)))]), vec![0]);

  // caller() { const o = {}; return helper(o); }
  let caller = func(
    cfg_single_block(vec![
      Inst::call(
        0,
        Arg::Builtin("__optimize_js_object".to_string()),
        Arg::Const(Const::Undefined),
        Vec::new(),
        Vec::new(),
      ),
      Inst::call(
        1,
        Arg::Fn(0),
        Arg::Const(Const::Undefined),
        vec![Arg::Var(0)],
        Vec::new(),
      ),
      Inst::ret(Some(Arg::Var(1))),
    ]),
    Vec::new(),
  );

  let program = Program {
    source_file: optimize_js::FileId(0),
    source_len: 0,
    functions: vec![helper, caller],
    top_level: func(cfg_single_block(Vec::new()), Vec::new()),
    top_level_mode: TopLevelMode::Module,
    symbols: None,
  };

  let summaries = interproc_escape::compute_program_escape_summaries(&program);
  let caller_escape = escape::analyze_cfg_escapes_with_params_and_summaries(
    &program.functions[1].body,
    &program.functions[1].params,
    Some(&summaries),
    None,
  );
  assert_eq!(escape_of(&caller_escape, 0), EscapeState::ReturnEscape);
}

#[test]
fn calling_helper_that_throws_param_marks_alloc_as_return_escape() {
  // helper(x) { throw x; }
  let helper = func(cfg_single_block(vec![Inst::throw(Arg::Var(0))]), vec![0]);

  // caller() { const o = {}; helper(o); }
  let caller = func(
    cfg_single_block(vec![
      Inst::call(
        0,
        Arg::Builtin("__optimize_js_object".to_string()),
        Arg::Const(Const::Undefined),
        Vec::new(),
        Vec::new(),
      ),
      Inst::call(
        None::<u32>,
        Arg::Fn(0),
        Arg::Const(Const::Undefined),
        vec![Arg::Var(0)],
        Vec::new(),
      ),
      Inst::ret(None),
    ]),
    Vec::new(),
  );

  let program = Program {
    source_file: optimize_js::FileId(0),
    source_len: 0,
    functions: vec![helper, caller],
    top_level: func(cfg_single_block(Vec::new()), Vec::new()),
    top_level_mode: TopLevelMode::Module,
    symbols: None,
  };

  let summaries = interproc_escape::compute_program_escape_summaries(&program);
  let caller_escape = escape::analyze_cfg_escapes_with_params_and_summaries(
    &program.functions[1].body,
    &program.functions[1].params,
    Some(&summaries),
    None,
  );
  assert_eq!(escape_of(&caller_escape, 0), EscapeState::ReturnEscape);
}

#[test]
fn wrapper_call_chain_propagates_thrown_param() {
  // helper(x) { throw x; }
  let helper = func(cfg_single_block(vec![Inst::throw(Arg::Var(0))]), vec![0]);

  // wrapper(x) { helper(x); return; }
  let wrapper = func(
    cfg_single_block(vec![
      Inst::call(
        None::<u32>,
        Arg::Fn(0),
        Arg::Const(Const::Undefined),
        vec![Arg::Var(0)],
        Vec::new(),
      ),
      Inst::ret(None),
    ]),
    vec![0],
  );

  // caller() { const o = {}; wrapper(o); }
  let caller = func(
    cfg_single_block(vec![
      Inst::call(
        0,
        Arg::Builtin("__optimize_js_object".to_string()),
        Arg::Const(Const::Undefined),
        Vec::new(),
        Vec::new(),
      ),
      Inst::call(
        None::<u32>,
        Arg::Fn(1),
        Arg::Const(Const::Undefined),
        vec![Arg::Var(0)],
        Vec::new(),
      ),
      Inst::ret(None),
    ]),
    Vec::new(),
  );

  let program = Program {
    source_file: optimize_js::FileId(0),
    source_len: 0,
    functions: vec![helper, wrapper, caller],
    top_level: func(cfg_single_block(Vec::new()), Vec::new()),
    top_level_mode: TopLevelMode::Module,
    symbols: None,
  };

  let summaries = interproc_escape::compute_program_escape_summaries(&program);
  assert!(summaries.functions[1].throws_param.contains(&0));

  let caller_escape = escape::analyze_cfg_escapes_with_params_and_summaries(
    &program.functions[2].body,
    &program.functions[2].params,
    Some(&summaries),
    None,
  );
  assert_eq!(escape_of(&caller_escape, 0), EscapeState::ReturnEscape);
}

#[test]
fn storing_into_local_receiver_arg_propagates_receiver_escape() {
  // helper(x, y) { y.p = x; return y; }
  let helper = func(
    cfg_single_block(vec![
      Inst::prop_assign(
        Arg::Var(1),
        Arg::Const(Const::Str("p".to_string())),
        Arg::Var(0),
      ),
      Inst::ret(Some(Arg::Var(1))),
    ]),
    vec![0, 1],
  );

  // caller() { const x = {}; const y = {}; return helper(x, y); }
  let caller = func(
    cfg_single_block(vec![
      Inst::call(
        0,
        Arg::Builtin("__optimize_js_object".to_string()),
        Arg::Const(Const::Undefined),
        Vec::new(),
        Vec::new(),
      ),
      Inst::call(
        1,
        Arg::Builtin("__optimize_js_object".to_string()),
        Arg::Const(Const::Undefined),
        Vec::new(),
        Vec::new(),
      ),
      Inst::call(
        2,
        Arg::Fn(0),
        Arg::Const(Const::Undefined),
        vec![Arg::Var(0), Arg::Var(1)],
        Vec::new(),
      ),
      Inst::ret(Some(Arg::Var(2))),
    ]),
    Vec::new(),
  );

  let program = Program {
    source_file: optimize_js::FileId(0),
    source_len: 0,
    functions: vec![helper, caller],
    top_level: func(cfg_single_block(Vec::new()), Vec::new()),
    top_level_mode: TopLevelMode::Module,
    symbols: None,
  };

  let summaries = interproc_escape::compute_program_escape_summaries(&program);
  let caller_escape = escape::analyze_cfg_escapes_with_params_and_summaries(
    &program.functions[1].body,
    &program.functions[1].params,
    Some(&summaries),
    None,
  );
  assert_eq!(escape_of(&caller_escape, 1), EscapeState::ReturnEscape);
  assert_eq!(
    escape_of(&caller_escape, 0),
    EscapeState::ReturnEscape,
    "expected value stored into returned receiver arg to be ReturnEscape (not GlobalEscape)"
  );
}

#[test]
fn helper_storing_param_to_global_forces_global_escape() {
  // helper(x) { unknownGlobal = x; }
  let helper = func(
    cfg_single_block(vec![
      Inst::unknown_store("unknownGlobal".to_string(), Arg::Var(0)),
      Inst::ret(None),
    ]),
    vec![0],
  );

  // caller() { const o = {}; helper(o); }
  let caller = func(
    cfg_single_block(vec![
      Inst::call(
        0,
        Arg::Builtin("__optimize_js_object".to_string()),
        Arg::Const(Const::Undefined),
        Vec::new(),
        Vec::new(),
      ),
      Inst::call(
        None::<u32>,
        Arg::Fn(0),
        Arg::Const(Const::Undefined),
        vec![Arg::Var(0)],
        Vec::new(),
      ),
      Inst::ret(None),
    ]),
    Vec::new(),
  );

  let program = Program {
    source_file: optimize_js::FileId(0),
    source_len: 0,
    functions: vec![helper, caller],
    top_level: func(cfg_single_block(Vec::new()), Vec::new()),
    top_level_mode: TopLevelMode::Module,
    symbols: None,
  };

  let summaries = interproc_escape::compute_program_escape_summaries(&program);
  let caller_escape = escape::analyze_cfg_escapes_with_params_and_summaries(
    &program.functions[1].body,
    &program.functions[1].params,
    Some(&summaries),
    None,
  );
  assert_eq!(escape_of(&caller_escape, 0), EscapeState::GlobalEscape);
}

#[test]
fn summaries_are_deterministic() {
  let helper = func(cfg_single_block(vec![Inst::ret(Some(Arg::Var(0)))]), vec![0]);
  let caller = func(
    cfg_single_block(vec![
      Inst::call(
        0,
        Arg::Builtin("__optimize_js_object".to_string()),
        Arg::Const(Const::Undefined),
        Vec::new(),
        Vec::new(),
      ),
      Inst::call(
        1,
        Arg::Fn(0),
        Arg::Const(Const::Undefined),
        vec![Arg::Var(0)],
        Vec::new(),
      ),
      Inst::ret(Some(Arg::Var(1))),
    ]),
    Vec::new(),
  );
  let program = Program {
    source_file: optimize_js::FileId(0),
    source_len: 0,
    functions: vec![helper, caller],
    top_level: func(cfg_single_block(Vec::new()), Vec::new()),
    top_level_mode: TopLevelMode::Module,
    symbols: None,
  };

  let a = interproc_escape::compute_program_escape_summaries(&program);
  let b = interproc_escape::compute_program_escape_summaries(&program);
  assert_eq!(a, b);
}
