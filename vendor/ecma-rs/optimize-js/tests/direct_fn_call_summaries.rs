#[path = "common/mod.rs"]
mod common;

use common::compile_source;
use optimize_js::analysis::{annotate_program, FunctionKey};
use optimize_js::analysis::escape::EscapeState;
use optimize_js::il::inst::{Arg, InstTyp, OwnershipState};
use optimize_js::TopLevelMode;

fn find_direct_fn_call<'a>(program: &'a optimize_js::Program, fn_id: usize) -> &'a optimize_js::il::inst::Inst {
  program
    .top_level
    .body
    .bblocks
    .all()
    .flat_map(|(_, block)| block.iter())
    .find(|inst| {
      inst.t == InstTyp::Call
        && matches!(inst.as_call().1, Arg::Fn(id) if *id == fn_id)
    })
    .expect("expected direct Arg::Fn call in top-level")
}

fn find_top_level_object_alloc(program: &optimize_js::Program) -> u32 {
  program
    .top_level
    .body
    .bblocks
    .all()
    .flat_map(|(_, block)| block.iter())
    .find_map(|inst| {
      if inst.t != InstTyp::Call {
        return None;
      }
      let (tgt, callee, _this, _args, _spreads) = inst.as_call();
      if matches!(callee, Arg::Builtin(name) if name == "__optimize_js_object") {
        tgt
      } else {
        None
      }
    })
    .expect("expected object literal allocation in top-level")
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
