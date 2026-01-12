use optimize_js::il::inst::InstTyp;
use optimize_js::{compile_source, TopLevelMode};

#[test]
fn exception_prune_removes_catch_when_try_body_cannot_throw() {
  let program = compile_source(
    r#"
      export const f = () => {
        try {
          const x = [1, 2, 3];
          return x;
        } catch (e) {
          return 0;
        }
      };
    "#,
    TopLevelMode::Module,
    false,
  )
  .expect("compile");

  assert_eq!(program.functions.len(), 1);
  let cfg = program.functions[0].analyzed_cfg();

  // The try body only constructs an array literal (internal helper) and should be proven
  // non-throwing; the catch handler and the invoke exception edge should be pruned.
  let mut saw_invoke = false;
  let mut saw_catch = false;
  for (_, block) in cfg.bblocks.all() {
    for inst in block {
      saw_invoke |= inst.t == InstTyp::Invoke;
      saw_catch |= inst.t == InstTyp::Catch;
    }
  }

  assert!(!saw_invoke, "expected Invoke to be rewritten to Call after pruning");
  assert!(!saw_catch, "expected catch handler blocks to be removed after pruning");
}

