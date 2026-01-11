use optimize_js::analysis::escape::EscapeState;
use optimize_js::compile_source;
use optimize_js::il::inst::{Arg, InstTyp};
use optimize_js::TopLevelMode;

fn find_object_alloc<'a>(
  cfg: &'a optimize_js::cfg::cfg::Cfg,
) -> Option<&'a optimize_js::il::inst::Inst> {
  cfg
    .bblocks
    .all()
    .flat_map(|(_, block)| block.iter())
    .find(|inst| {
      inst.t == InstTyp::Call
        && matches!(inst.args.get(0), Some(Arg::Builtin(name)) if name == "__optimize_js_object")
    })
}

#[test]
fn ssa_escape_uses_canonical_param_order_for_arg_escape() {
  let program = compile_source(
    r#"
      function f(a, b) {
        const o = {};
        b.x = o;
        return 0;
      }
    "#,
    TopLevelMode::Module,
    false,
  )
  .expect("compile");

  assert_eq!(program.functions.len(), 1, "expected exactly one nested function");
  let func = &program.functions[0];
  let ssa_cfg = func.ssa_body.as_ref().expect("expected SSA cfg to be preserved");

  let alloc = find_object_alloc(ssa_cfg).expect("object allocation call should exist in SSA cfg");
  assert_eq!(
    alloc.meta.result_escape,
    Some(EscapeState::ArgEscape(1)),
    "expected allocation stored into second parameter to be ArgEscape(1)"
  );
  assert_ne!(
    alloc.meta.result_escape,
    Some(EscapeState::ArgEscape(0)),
    "expected allocation escape metadata to not use inferred param ordering"
  );
}

#[test]
fn ssa_escape_first_param_is_arg_escape_zero() {
  let program = compile_source(
    r#"
      function f(a, b) {
        const o = {};
        a.x = o;
        return 0;
      }
    "#,
    TopLevelMode::Module,
    false,
  )
  .expect("compile");

  assert_eq!(program.functions.len(), 1, "expected exactly one nested function");
  let func = &program.functions[0];
  let ssa_cfg = func.ssa_body.as_ref().expect("expected SSA cfg to be preserved");

  let alloc = find_object_alloc(ssa_cfg).expect("object allocation call should exist in SSA cfg");
  assert_eq!(
    alloc.meta.result_escape,
    Some(EscapeState::ArgEscape(0)),
    "expected allocation stored into first parameter to be ArgEscape(0)"
  );
}

