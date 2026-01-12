use optimize_js::il::inst::{Arg, Const, InstTyp};
use optimize_js::{CompileCfgOptions, TopLevelMode};

#[test]
fn regex_literal_lowers_to_regex_lit_inst() {
  let program = optimize_js::compile_source_with_cfg_options(
    "void /a+/;",
    TopLevelMode::Module,
    false,
    CompileCfgOptions {
      run_opt_passes: false,
      ..CompileCfgOptions::default()
    },
  )
  .expect("compile source");

  let mut saw_regex_lit = false;
  let mut saw_marker_call = false;

  for (_, block) in program.top_level.body.bblocks.all() {
    for inst in block.iter() {
      match inst.t {
        InstTyp::RegexLit => {
          saw_regex_lit = true;
          let (_tgt, regex_arg) = inst.as_regex_lit();
          assert!(
            matches!(regex_arg, Arg::Const(Const::Str(s)) if s.contains("a+")),
            "expected RegexLit payload to contain pattern, got {regex_arg:?}"
          );
        }
        InstTyp::Call => {
          let (_tgt, callee, _this, _args, _spreads) = inst.as_call();
          if matches!(callee, Arg::Builtin(name) if name == "__optimize_js_regex") {
            saw_marker_call = true;
          }
        }
        _ => {}
      }
    }
  }

  assert!(saw_regex_lit, "expected InstTyp::RegexLit in lowered IL");
  assert!(
    !saw_marker_call,
    "did not expect `__optimize_js_regex` marker builtin call in IL"
  );
}

