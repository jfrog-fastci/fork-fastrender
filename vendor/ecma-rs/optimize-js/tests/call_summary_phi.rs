use optimize_js::analysis::call_summary::{summarize_program, ReturnKind};
use optimize_js::analysis::escape::EscapeState;
use optimize_js::il::inst::InstTyp;
use optimize_js::{compile_source_with_cfg_options, CompileCfgOptions, TopLevelMode};

#[test]
fn call_summary_propagates_param_origin_through_phi() {
  // This program forces SSA phi insertion for `y`, but both incoming values
  // ultimately originate from the same parameter `x`.
  //
  // The call summary analysis should therefore classify the return as
  // `AliasParam(1)`, rather than falling back to `Unknown`.
  let source = r#"
    function f(cond, x) {
      let y;
      if (cond) {
        y = x;
      } else {
        let tmp = x;
        y = tmp;
      }
      return y;
    }
    f(unknown_cond(), {});
  "#;

  // Keep SSA + phi nodes intact so this test exercises phi handling in the
  // call-summary analysis.
  let program = compile_source_with_cfg_options(
    source,
    TopLevelMode::Module,
    false,
    CompileCfgOptions {
      keep_ssa: false,
      run_opt_passes: false,
    },
  )
  .expect("compile");

  assert_eq!(program.functions.len(), 1, "expected exactly one nested function");
  let func = &program.functions[0];
  let has_phi = func
    .cfg_ssa()
    .expect("expected SSA cfg to be preserved")
    .bblocks
    .all()
    .any(|(_, block)| block.iter().any(|inst| inst.t == InstTyp::Phi));
  assert!(has_phi, "expected SSA cfg to contain at least one Phi inst");

  let summaries = summarize_program(&program);
  assert_eq!(summaries.len(), 1, "summaries should align with program.functions");

  let summary = &summaries[0];
  assert_eq!(
    summary.return_kind,
    ReturnKind::AliasParam(1),
    "expected return value to alias `x` (parameter index 1)"
  );
  assert_eq!(
    summary.param_escape[1],
    EscapeState::ReturnEscape,
    "expected returned parameter to be marked as ReturnEscape"
  );
}
