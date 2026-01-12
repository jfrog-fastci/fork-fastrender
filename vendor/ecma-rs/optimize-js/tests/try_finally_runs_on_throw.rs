use optimize_js::dom::Dom;
use optimize_js::il::inst::{Arg, Const, InstTyp};
use optimize_js::{compile_source_with_cfg_options, CompileCfgOptions, TopLevelMode};
use parse_js::num::JsNumber;
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
fn throw_inside_try_is_lowered_through_finally() {
  let program = compile_source_with_cfg_options(
    r#"
      export const f = () => {
        try {
          throw 1;
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

  // The `throw` inside the try block should target an in-function landingpad handler.
  let mut found_throw_to = None;
  for label in cfg.graph.labels_sorted() {
    let Some(inst) = cfg.bblocks.get(label).last() else {
      continue;
    };
    if inst.t != InstTyp::Throw || inst.labels.len() != 1 {
      continue;
    }
    assert!(
      matches!(inst.as_throw(), Arg::Const(Const::Num(JsNumber(1.0)))),
      "expected throw 1 in try block"
    );
    found_throw_to = Some((label, inst.labels[0]));
    break;
  }
  let (throw_label, landingpad_label) = found_throw_to.expect("expected throw_to in try block");
  assert_eq!(
    cfg.graph.children_sorted(throw_label),
    vec![landingpad_label],
    "throw_to should branch directly to the landingpad"
  );

  // Landingpad should begin by materializing the exception via `Catch`.
  let landingpad_block = cfg.bblocks.get(landingpad_label);
  let first_non_phi = landingpad_block
    .iter()
    .position(|inst| inst.t != InstTyp::Phi)
    .expect("landingpad should contain a Catch instruction");
  assert_eq!(landingpad_block[first_non_phi].t, InstTyp::Catch);

  // Landingpad should funnel into the finally block.
  let landingpad_children = cfg.graph.children_sorted(landingpad_label);
  assert_eq!(landingpad_children.len(), 1);
  let finally_label = landingpad_children[0];

  // After finally executes, the exception should be re-thrown (as a plain `Throw`).
  let mut visited = std::collections::HashSet::new();
  let mut q = VecDeque::from([finally_label]);
  let mut saw_throw = false;
  while let Some(label) = q.pop_front() {
    if !visited.insert(label) {
      continue;
    }
    if let Some(inst) = cfg.bblocks.get(label).last() {
      if inst.t == InstTyp::Throw && inst.labels.is_empty() {
        saw_throw = true;
        break;
      }
    }
    for child in cfg.graph.children_sorted(label) {
      q.push_back(child);
    }
  }
  assert!(saw_throw, "expected a plain throw after running finally");
}

#[test]
fn finally_body_executes_on_throw() {
  let program = compile_source_with_cfg_options(
    r#"
      export const f = (touch) => {
        try {
          throw 1;
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

  let rethrow_labels: Vec<u32> = reachable
    .iter()
    .copied()
    .filter(|label| {
      cfg
        .bblocks
        .get(*label)
        .last()
        .is_some_and(|inst| inst.t == InstTyp::Throw && inst.labels.is_empty())
    })
    .collect();
  assert!(
    !rethrow_labels.is_empty(),
    "expected to find a reachable rethrow after finally"
  );

  let guarded = rethrow_labels.iter().any(|&throw_label| {
    touch_call_labels
      .iter()
      .any(|&call_label| dom_bys.dominated_by(throw_label, call_label))
  });
  assert!(
    guarded,
    "expected rethrow to be dominated by a finally-body touch() call; touch_call_labels={touch_call_labels:?}, rethrow_labels={rethrow_labels:?}"
  );
}
