use optimize_js::il::inst::{Arg, InstTyp};
use optimize_js::{CompileCfgOptions, TopLevelMode};

#[test]
fn object_literal_lowers_to_object_lit_inst() {
  let program = optimize_js::compile_source_with_cfg_options(
    "void ({x:1, ...y});",
    TopLevelMode::Module,
    false,
    CompileCfgOptions {
      run_opt_passes: false,
      ..CompileCfgOptions::default()
    },
  )
  .expect("compile source");

  let mut saw_object_lit = false;
  let mut saw_marker_call = false;
  let mut args_len: Option<usize> = None;
  let mut first_marker: Option<String> = None;
  let mut second_marker: Option<String> = None;

  for (_, block) in program.top_level.body.bblocks.all() {
    for inst in block.iter() {
      match inst.t {
        InstTyp::ObjectLit => {
          saw_object_lit = true;
          let (_tgt, args) = inst.as_object_lit();
          args_len = Some(args.len());
          if let Some(Arg::Builtin(marker)) = args.get(0) {
            first_marker = Some(marker.clone());
          }
          if let Some(Arg::Builtin(marker)) = args.get(3) {
            second_marker = Some(marker.clone());
          }
        }
        InstTyp::Call => {
          let (_tgt, callee, _this, _args, _spreads) = inst.as_call();
          if matches!(callee, Arg::Builtin(name) if name == "__optimize_js_object") {
            saw_marker_call = true;
          }
        }
        _ => {}
      }
    }
  }

  assert!(saw_object_lit, "expected InstTyp::ObjectLit in lowered IL");
  assert_eq!(args_len, Some(6), "expected two object members encoded as 2x3 args");
  assert_eq!(
    first_marker.as_deref(),
    Some("__optimize_js_object_prop"),
    "expected first object member to use prop marker"
  );
  assert_eq!(
    second_marker.as_deref(),
    Some("__optimize_js_object_spread"),
    "expected second object member to use spread marker"
  );
  assert!(
    !saw_marker_call,
    "did not expect `__optimize_js_object` marker builtin call in IL"
  );
}

