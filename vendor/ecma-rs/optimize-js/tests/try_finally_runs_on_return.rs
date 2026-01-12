use optimize_js::dom::Dom;
use optimize_js::il::inst::{Arg, InstTyp};
use optimize_js::{compile_source_with_cfg_options, CompileCfgOptions, TopLevelMode};
use std::collections::VecDeque;

fn reachable_labels(cfg: &optimize_js::cfg::cfg::Cfg) -> Vec<u32> {
  let mut seen = std::collections::BTreeSet::new();
  let mut out = Vec::new();
  let mut queue = VecDeque::new();
  queue.push_back(cfg.entry);
  while let Some(label) = queue.pop_front() {
    if !seen.insert(label) {
      continue;
    }
    out.push(label);
    for succ in cfg.graph.children_sorted(label) {
      queue.push_back(succ);
    }
  }
  out.sort_unstable();
  out
}

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

#[test]
fn finally_body_executes_on_return() {
  let program = compile_source_with_cfg_options(
    r#"
      export const f = (touch) => {
        try {
          return 1;
        } finally {
          touch();
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
  let touch_param = func.params[0];
  let cfg = func.analyzed_cfg();

  let dom: Dom = Dom::calculate(cfg);
  let dom_bys = dom.dominated_by_graph();

  let reachable = reachable_labels(cfg);
  let touch_call_labels: Vec<u32> = reachable
    .iter()
    .copied()
    .filter(|label| {
      cfg.bblocks.get(*label).iter().any(|inst| {
        matches!(inst.t, InstTyp::Call | InstTyp::Invoke)
          && inst.args.get(0) == Some(&Arg::Var(touch_param))
      })
    })
    .collect();
  assert!(
    !touch_call_labels.is_empty(),
    "expected to find a reachable call to touch() in lowered finally body"
  );

  let return_labels: Vec<u32> = reachable
    .iter()
    .copied()
    .filter(|label| {
      cfg
        .bblocks
        .get(*label)
        .last()
        .is_some_and(|inst| inst.t == InstTyp::Return)
    })
    .collect();
  assert!(
    !return_labels.is_empty(),
    "expected to find a reachable Return terminator"
  );

  let guarded = return_labels.iter().any(|&ret_label| {
    touch_call_labels
      .iter()
      .any(|&call_label| dom_bys.dominated_by(ret_label, call_label))
  });
  assert!(
    guarded,
    "expected Return to be dominated by a finally-body touch() call; touch_call_labels={touch_call_labels:?}, return_labels={return_labels:?}"
  );
}
