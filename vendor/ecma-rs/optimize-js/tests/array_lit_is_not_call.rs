use optimize_js::il::inst::{Arg, InstTyp};
use optimize_js::{CompileCfgOptions, TopLevelMode};

#[test]
fn array_literal_lowers_to_array_lit_inst() {
  let program = optimize_js::compile_source_with_cfg_options(
    "void [1, ...xs];",
    TopLevelMode::Module,
    false,
    CompileCfgOptions {
      run_opt_passes: false,
      ..CompileCfgOptions::default()
    },
  )
  .expect("compile source");

  let mut saw_array_lit = false;
  let mut saw_marker_call = false;
  let mut spreads: Option<Vec<usize>> = None;

  for (_, block) in program.top_level.body.bblocks.all() {
    for inst in block.iter() {
      match inst.t {
        InstTyp::ArrayLit => {
          saw_array_lit = true;
          let (_tgt, args, spreads_slice) = inst.as_array_lit();
          // `[1, ...xs]` should have two elements, with the second being spread.
          assert_eq!(args.len(), 2, "expected two array literal elements");
          spreads = Some(spreads_slice.to_vec());
        }
        InstTyp::Call => {
          let (_tgt, callee, _this, _args, _spreads) = inst.as_call();
          if matches!(callee, Arg::Builtin(name) if name == "__optimize_js_array") {
            saw_marker_call = true;
          }
        }
        _ => {}
      }
    }
  }

  assert!(saw_array_lit, "expected InstTyp::ArrayLit in lowered IL");
  assert_eq!(
    spreads,
    Some(vec![1]),
    "expected spread indices to be relative to ArrayLit.args (0-based)"
  );
  assert!(
    !saw_marker_call,
    "did not expect `__optimize_js_array` marker builtin call in IL"
  );
}

