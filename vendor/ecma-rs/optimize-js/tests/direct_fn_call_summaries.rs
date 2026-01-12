#[path = "common/mod.rs"]
mod common;

use common::compile_source;
use optimize_js::analysis::{annotate_program, FunctionKey};
use optimize_js::analysis::escape::EscapeState;
use optimize_js::il::inst::{Arg, InstTyp, OwnershipState};
use optimize_js::TopLevelMode;

fn find_direct_fn_call<'a>(program: &'a optimize_js::Program, fn_id: usize) -> &'a optimize_js::il::inst::Inst {
  // `annotate_program` writes analysis-derived instruction metadata (ownership/escape/etc) to the
  // CFG returned by `ProgramFunction::analyzed_cfg()` (SSA form when available), not necessarily
  // to `ProgramFunction::body` (which is SSA-deconstructed by default).
  program
    .top_level
    .analyzed_cfg()
    .bblocks
    .all()
    .flat_map(|(_, block)| block.iter())
    .find(|inst| inst.t == InstTyp::Call && matches!(inst.as_call().1, Arg::Fn(id) if *id == fn_id))
    .expect("expected direct Arg::Fn call in analyzed top-level CFG")
}

fn find_top_level_object_alloc(program: &optimize_js::Program) -> u32 {
  program
    .top_level
    .analyzed_cfg()
    .bblocks
    .all()
    .flat_map(|(_, block)| block.iter())
    .find_map(|inst| {
      (inst.t == InstTyp::ObjectLit)
        .then(|| inst.tgts.get(0).copied())
        .flatten()
    })
    .expect("expected object literal allocation in analyzed top-level CFG")
}

fn find_fn_object_alloc(program: &optimize_js::Program, fn_id: usize) -> u32 {
  program
    .functions
    .get(fn_id)
    .expect("missing function")
    .analyzed_cfg()
    .bblocks
    .all()
    .flat_map(|(_, block)| block.iter())
    .find_map(|inst| {
      (inst.t == InstTyp::ObjectLit)
        .then(|| inst.tgts.get(0).copied())
        .flatten()
    })
    .expect("expected object literal allocation in analyzed function CFG")
}

#[test]
fn direct_fn_call_return_fresh_alloc_is_owned() {
  let src = r#"
    const make = () => ({x:1});
    const v = make();
    // Ensure the result is used by an impure instruction so trivial DCE doesn't
    // strip the call target.
    v.x = 2;
  "#;

  let mut program = compile_source(src, TopLevelMode::Module, false);
  annotate_program(&mut program);

  let call = find_direct_fn_call(&program, 0);
  assert_eq!(
    call.meta.ownership,
    OwnershipState::Owned,
    "expected call result to be Owned, got {:?}",
    call.meta.ownership
  );
}

#[test]
fn direct_fn_call_return_fresh_alloc_through_phi_is_owned() {
  let src = r#"
    const make = () => {
      let v;
      if (unknown_cond()) {
        v = {x:1};
      } else {
        v = {x:2};
      }
      return v;
    };
    const out = make();
    // Ensure the call result is observed by an impure instruction so trivial DCE
    // does not eliminate the call target.
    out.x = 2;
  "#;
 
  let mut program = compile_source(src, TopLevelMode::Module, false);
  annotate_program(&mut program);
 
  let call = find_direct_fn_call(&program, 0);
  assert_eq!(
    call.meta.ownership,
    OwnershipState::Owned,
    "expected call result to be Owned when callee returns fresh alloc via Phi, got {:?}",
    call.meta.ownership
  );
}

#[test]
fn direct_fn_call_return_fresh_alloc_with_spread_is_owned() {
  let src = r#"
    const make = (a) => {
      return [...a];
    };
    const a = [];
    const out = make(a);
    out[0] = 2;
  "#;

  let mut program = compile_source(src, TopLevelMode::Module, false);
  annotate_program(&mut program);

  let call = find_direct_fn_call(&program, 0);
  assert_eq!(
    call.meta.ownership,
    OwnershipState::Owned,
    "expected call result to be Owned when callee returns fresh alloc array literal with spread, got {:?}",
    call.meta.ownership
  );
}

#[test]
fn direct_fn_call_return_fresh_alloc_with_spread_args_is_owned() {
  let src = r#"
    const make = () => ({x:1});
    const args = [];
    const out = make(...args);
    out.x = 2;
  "#;

  let mut program = compile_source(src, TopLevelMode::Module, false);
  annotate_program(&mut program);

  let call = find_direct_fn_call(&program, 0);
  assert_eq!(
    call.meta.ownership,
    OwnershipState::Owned,
    "expected call result to be Owned when direct Arg::Fn call uses spread args, got {:?}",
    call.meta.ownership
  );
}

#[test]
fn direct_fn_call_return_fresh_alloc_through_spread_call_is_owned() {
  let src = r#"
    const g = () => ({x:1});
    const f = (...args) => g(...args);
    const out = f();
    out.x = 2;
  "#;

  let mut program = compile_source(src, TopLevelMode::Module, false);
  annotate_program(&mut program);

  // g is function 0, f is function 1.
  let call = find_direct_fn_call(&program, 1);
  assert_eq!(
    call.meta.ownership,
    OwnershipState::Owned,
    "expected call result to be Owned when callee returns fresh alloc via spread call, got {:?}",
    call.meta.ownership
  );
}

#[test]
fn direct_fn_call_param_escape_is_propagated() {
  let src = r#"
    const f = (a) => { globalSink(a); };
    const o = {};
    f(o);
  "#;

  let mut program = compile_source(src, TopLevelMode::Module, false);
  let analyses = annotate_program(&mut program);

  let alloc = find_top_level_object_alloc(&program);
  let escape = analyses
    .escape
    .get(&FunctionKey::TopLevel)
    .expect("escape results for top-level");
  assert_eq!(
    escape.get(&alloc),
    Some(&EscapeState::GlobalEscape),
    "expected argument to escape via direct Arg::Fn call"
  );
}

#[test]
fn direct_fn_call_param_no_escape_is_respected() {
  let src = r#"
     const f = (a) => { a; };
     const o = {};
     f(o);
   "#;

  let mut program = compile_source(src, TopLevelMode::Module, false);
  let analyses = annotate_program(&mut program);

  let alloc = find_top_level_object_alloc(&program);
  let escape = analyses
    .escape
    .get(&FunctionKey::TopLevel)
    .expect("escape results for top-level");

  assert_eq!(
    escape.get(&alloc),
    Some(&EscapeState::NoEscape),
    "expected allocation to remain local when callee param does not escape"
  );
}

#[test]
fn direct_fn_call_param_no_escape_is_respected_with_spread_args() {
  let src = r#"
    const f = (a, b) => { a; b; };
    const o = {};
    const args = [];
    f(o, ...args);
  "#;

  let mut program = compile_source(src, TopLevelMode::Module, false);
  let analyses = annotate_program(&mut program);

  let alloc = find_top_level_object_alloc(&program);
  let escape = analyses
    .escape
    .get(&FunctionKey::TopLevel)
    .expect("escape results for top-level");

  assert_eq!(
    escape.get(&alloc),
    Some(&EscapeState::NoEscape),
    "expected allocation to remain local when callee param does not escape (even with spread args)"
  );
}

#[test]
fn direct_fn_call_param_escape_is_propagated_with_spread_before_arg() {
  let src = r#"
    const g = (x, y) => { globalSink(x); y; };
    const f = (a, arr) => { g(...arr, a); };
    const o = {};
    const arr = unknownArray();
    f(o, arr);
  "#;

  let mut program = compile_source(src, TopLevelMode::Module, false);
  let analyses = annotate_program(&mut program);

  let alloc = find_top_level_object_alloc(&program);
  let escape = analyses
    .escape
    .get(&FunctionKey::TopLevel)
    .expect("escape results for top-level");

  assert_eq!(
    escape.get(&alloc),
    Some(&EscapeState::GlobalEscape),
    "expected allocation to escape when passed after a spread (it may flow into an escaping callee parameter)"
  );
}

#[test]
fn direct_fn_call_param_no_escape_through_captured_callee_is_respected() {
  let src = r#"
    const g = (x) => { x; };
    const f = (a) => { g(a); };
    const o = {};
    f(o);
  "#;

  let mut program = compile_source(src, TopLevelMode::Module, false);
  let analyses = annotate_program(&mut program);

  let alloc = find_top_level_object_alloc(&program);
  let escape = analyses
    .escape
    .get(&FunctionKey::TopLevel)
    .expect("escape results for top-level");

  assert_eq!(
    escape.get(&alloc),
    Some(&EscapeState::NoEscape),
    "expected allocation to remain local when param only flows into a captured callee that does not escape it"
  );
}

#[test]
fn captured_callee_call_does_not_force_local_alloc_global_escape() {
  let src = r#"
    const g = (x) => { x; };
    const f = () => { const o = {}; g(o); };
    f();
  "#;

  let mut program = compile_source(src, TopLevelMode::Module, false);
  let analyses = annotate_program(&mut program);

  // g is function 0, f is function 1.
  let alloc = find_fn_object_alloc(&program, 1);
  let escape = analyses
    .escape
    .get(&FunctionKey::Fn(1))
    .expect("escape results for f");

  assert_eq!(
    escape.get(&alloc),
    Some(&EscapeState::NoEscape),
    "expected allocation passed to captured constant callee to remain local"
  );
}
