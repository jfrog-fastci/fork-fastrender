use optimize_js::il::inst::{Arg, Const, InstTyp};
use optimize_js::{compile_source_with_cfg_options, CompileCfgOptions, TopLevelMode};
use parse_js::num::JsNumber;

#[test]
fn try_catch_binds_thrown_value() {
  let program = compile_source_with_cfg_options(
    r#"
      export const f = () => {
        try {
          throw 1;
        } catch (e) {
          return e;
        }
      };
    "#,
    TopLevelMode::Module,
    false,
    CompileCfgOptions {
      keep_ssa: true,
      run_opt_passes: false,
      ..Default::default()
    },
  )
  .expect("compile");

  assert_eq!(program.functions.len(), 1);
  let func = &program.functions[0];
  let cfg = func.analyzed_cfg();

  let mut found_throw = None;
  for label in cfg.graph.labels_sorted() {
    let Some(inst) = cfg.bblocks.get(label).last() else {
      continue;
    };
    if inst.t != InstTyp::Throw || inst.labels.len() != 1 {
      continue;
    }

    let value = inst.as_throw();
    assert!(
      matches!(value, Arg::Const(Const::Num(JsNumber(1.0)))),
      "expected throw 1 in try block"
    );
    found_throw = Some((label, inst.labels[0]));
    break;
  }

  let (throw_label, catch_label) = found_throw.expect("expected throw_to handler edge");
  assert_eq!(
    cfg.graph.children_sorted(throw_label),
    vec![catch_label],
    "throw_to should transfer control directly to catch handler"
  );

  let catch_block = cfg
    .bblocks
    .maybe_get(catch_label)
    .expect("missing catch handler block");
  let first_non_phi = catch_block
    .iter()
    .position(|inst| inst.t != InstTyp::Phi)
    .expect("catch handler should contain a Catch instruction");
  assert_eq!(catch_block[first_non_phi].t, InstTyp::Catch);

  let catch_tmp = catch_block[first_non_phi].as_catch();
  let catch_used = catch_block[first_non_phi + 1..]
    .iter()
    .flat_map(|inst| inst.args.iter())
    .any(|arg| matches!(arg, Arg::Var(v) if *v == catch_tmp));
  assert!(
    catch_used,
    "expected catch temp to be used to bind catch parameter"
  );
}
