use optimize_js::il::inst::InstTyp;
use optimize_js::{compile_source_with_cfg_options, CompileCfgOptions, TopLevelMode};
use std::collections::VecDeque;

#[test]
fn return_inside_try_is_lowered_through_finally() {
  let program = compile_source_with_cfg_options(
    r#"
      export const f = () => {
        try {
          return 1;
        } finally {
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
  let cfg = program.functions[0].analyzed_cfg();

  // The return statement in the try block should not lower to a `Return` terminator directly.
  assert!(
    cfg.bblocks
      .get(cfg.entry)
      .iter()
      .all(|inst| inst.t != InstTyp::Return),
    "expected try/finally return to be deferred (no Return in entry block)"
  );

  // The entry block should jump into the finally dispatch.
  let entry_children = cfg.graph.children_sorted(cfg.entry);
  assert_eq!(entry_children.len(), 1);
  let finally_label = entry_children[0];
  assert_ne!(finally_label, cfg.entry);

  let finally_block = cfg.bblocks.get(finally_label);
  assert!(
    finally_block.iter().any(|inst| inst.t == InstTyp::CondGoto),
    "expected finally dispatch to contain conditional branches"
  );

  // A return should be reachable from the finally block.
  let mut visited = std::collections::HashSet::new();
  let mut q = VecDeque::from([finally_label]);
  let mut saw_return = false;
  while let Some(label) = q.pop_front() {
    if !visited.insert(label) {
      continue;
    }
    if cfg.bblocks.get(label).iter().any(|inst| inst.t == InstTyp::Return) {
      saw_return = true;
      break;
    }
    for child in cfg.graph.children_sorted(label) {
      q.push_back(child);
    }
  }
  assert!(saw_return, "expected a return path after running finally");
}
