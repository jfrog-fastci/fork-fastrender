use optimize_js::il::inst::{Arg, Const, InstTyp};
use optimize_js::{compile_source_with_cfg_options, CompileCfgOptions, TopLevelMode};
use parse_js::num::JsNumber;

fn collect_calls(cfg: &optimize_js::cfg::cfg::Cfg) -> Vec<&optimize_js::il::inst::Inst> {
  cfg
    .bblocks
    .all()
    .flat_map(|(_, block)| block.iter())
    .filter(|inst| inst.t == InstTyp::Call)
    .collect()
}

fn compile_with_devirtualize(source: &str) -> optimize_js::Program {
  compile_source_with_cfg_options(
    source,
    TopLevelMode::Module,
    false,
    CompileCfgOptions {
      enable_devirtualize: true,
      ..CompileCfgOptions::default()
    },
  )
  .expect("compile")
}

#[test]
fn devirtualizes_const_var_call_to_direct_fn() {
  let src = r#"
    function foo(x) { return x; }
    const f = foo;
    f(1);
  "#;

  let program = compile_with_devirtualize(src);
  let calls = collect_calls(program.top_level.analyzed_cfg());
  assert!(
    calls.iter().any(|inst| matches!(inst.as_call().1, Arg::Fn(0))),
    "expected indirect call through const var to be rewritten to Arg::Fn(0); calls: {calls:?}"
  );
}

#[test]
fn devirtualizes_non_escaping_object_field_call_to_direct_fn() {
  let src = r#"
    function foo(x) { return x; }
    const obj = {};
    obj.m = foo;
    obj.m(1);
  "#;

  let program = compile_with_devirtualize(src);
  let calls = collect_calls(program.top_level.analyzed_cfg());
  assert!(
    calls.iter().any(|inst| matches!(inst.as_call().1, Arg::Fn(0))),
    "expected indirect call through non-escaping obj field to be rewritten to Arg::Fn(0); calls: {calls:?}"
  );
}

#[test]
fn does_not_devirtualize_when_multiple_possible_callees() {
  let src = r#"
    function foo(x) { return x; }
    function bar(x) { return x + 1; }
    let f;
    if (unknown_cond()) {
      f = foo;
      globalSink(0);
    } else {
      f = bar;
      globalSink(0);
    }
    f(1);
  "#;

  let program = compile_with_devirtualize(src);
  let calls = collect_calls(program.top_level.analyzed_cfg());
  let f_call = calls
    .iter()
    .find(|inst| {
      let (_tgt, _callee, _this, args, _spreads) = inst.as_call();
      matches!(
        args,
        [Arg::Const(Const::Num(JsNumber(n)))] if *n == 1.0
      )
    })
    .expect("expected call to f(1) in top-level CFG");
  assert!(
    matches!(f_call.as_call().1, Arg::Var(_)),
    "expected call to remain indirect when multiple possible callees; call: {f_call:?}"
  );
}

#[test]
fn does_not_devirtualize_object_field_after_unknown_call_that_may_mutate_it() {
  let src = r#"
    function foo(x) { return x; }
    function bar(x) { return x + 1; }
    const obj = {};
    obj.m = foo;
    unknown_mutate(obj);
    obj.m(1);
  "#;

  let program = compile_with_devirtualize(src);
  let calls = collect_calls(program.top_level.analyzed_cfg());
  let m_call = calls
    .iter()
    .find(|inst| {
      let (_tgt, _callee, _this, args, _spreads) = inst.as_call();
      matches!(
        args,
        [Arg::Const(Const::Num(JsNumber(n)))] if *n == 1.0
      )
    })
    .expect("expected call to obj.m(1) in top-level CFG");
  assert!(
    matches!(m_call.as_call().1, Arg::Var(_)),
    "expected call to remain indirect when obj may have been mutated by an unknown call; call: {m_call:?}"
  );
}
