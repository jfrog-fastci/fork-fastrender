use optimize_js::il::inst::{Arg, Const, InstTyp};
use optimize_js::{compile_source_with_cfg_options, CompileCfgOptions, TopLevelMode};
use parse_js::num::JsNumber;

fn compile_cfg(source: &str) -> optimize_js::cfg::cfg::Cfg {
  let program = compile_source_with_cfg_options(
    source,
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
  program.functions[0].analyzed_cfg().clone()
}

#[test]
fn break_inside_try_finally_is_lowered_through_finally() {
  let cfg = compile_cfg(
    r#"
      export const f = () => {
        let i = 0;
        while (i < 2) {
          try {
            break;
          } finally {
            i = 1;
          }
        }
        return i;
      };
    "#,
  );

  // The first jump completion code for a finally block is 3.
  let mut found = None;
  for label in cfg.graph.labels_sorted() {
    for inst in cfg.bblocks.get(label) {
      if inst.t == InstTyp::VarAssign
        && matches!(inst.args.as_slice(), [Arg::Const(Const::Num(JsNumber(3.0)))])
      {
        found = Some(label);
        break;
      }
    }
    if found.is_some() {
      break;
    }
  }
  let completion_block = found.expect("expected completion-kind assignment for break");
  let children = cfg.graph.children_sorted(completion_block);
  assert_eq!(
    children.len(),
    1,
    "break completion block should have exactly one successor"
  );
  let finally_label = children[0];
  assert!(
    cfg.bblocks
      .get(finally_label)
      .iter()
      .any(|inst| inst.t == InstTyp::CondGoto),
    "expected finally dispatch to contain conditional branches for jump completion"
  );
}

#[test]
fn continue_inside_try_finally_is_lowered_through_finally() {
  let cfg = compile_cfg(
    r#"
      export const f = () => {
        let i = 0;
        while (i < 2) {
          try {
            i = i + 1;
            continue;
          } finally {
          }
        }
        return i;
      };
    "#,
  );

  // The first jump completion code for a finally block is 3.
  let mut found = None;
  for label in cfg.graph.labels_sorted() {
    for inst in cfg.bblocks.get(label) {
      if inst.t == InstTyp::VarAssign
        && matches!(inst.args.as_slice(), [Arg::Const(Const::Num(JsNumber(3.0)))])
      {
        found = Some(label);
        break;
      }
    }
    if found.is_some() {
      break;
    }
  }
  let completion_block = found.expect("expected completion-kind assignment for continue");
  let children = cfg.graph.children_sorted(completion_block);
  assert_eq!(
    children.len(),
    1,
    "continue completion block should have exactly one successor"
  );
  let finally_label = children[0];
  assert!(
    cfg.bblocks
      .get(finally_label)
      .iter()
      .any(|inst| inst.t == InstTyp::CondGoto),
    "expected finally dispatch to contain conditional branches for jump completion"
  );
}
