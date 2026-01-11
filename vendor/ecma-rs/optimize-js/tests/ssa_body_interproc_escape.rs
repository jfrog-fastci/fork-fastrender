use optimize_js::analysis::escape::EscapeState;
use optimize_js::compile_source;
use optimize_js::il::inst::{Arg, InstTyp};
use optimize_js::TopLevelMode;

fn find_direct_fn_call_with_result<'a>(
  cfg: &'a optimize_js::cfg::cfg::Cfg,
) -> Option<&'a optimize_js::il::inst::Inst> {
  cfg
    .bblocks
    .all()
    .flat_map(|(_, block)| block.iter())
    .find(|inst| {
      if inst.t != InstTyp::Call {
        return false;
      }
      let (tgt, callee, _this, _args, spreads) = inst.as_call();
      tgt.is_some() && spreads.is_empty() && matches!(callee, Arg::Fn(_))
    })
}

#[test]
fn ssa_escape_uses_call_summaries_to_track_fresh_alloc_returned_from_callee() {
  // The inner function returns a fresh array allocation. The outer function calls it and returns
  // the result. For `result_escape` to be populated on the call instruction, SSA escape analysis
  // must use call summaries to classify the call result as a fresh local allocation.
  let program = compile_source(
    r#"
      const caller = () => {
        return (() => [1, 2])();
      };
      caller();
    "#,
    TopLevelMode::Module,
    false,
  )
  .expect("compile");

  let call = program
    .functions
    .iter()
    .find_map(|func| func.ssa_body.as_ref().and_then(find_direct_fn_call_with_result))
    .expect("expected a direct Arg::Fn call with a result in a nested function SSA cfg");

  assert_eq!(
    call.meta.result_escape,
    Some(EscapeState::ReturnEscape),
    "expected call result to be marked as ReturnEscape"
  );
}

