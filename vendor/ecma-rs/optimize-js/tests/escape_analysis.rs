#[path = "common/mod.rs"]
mod common;

use common::compile_source;
use optimize_js::analysis::escape::{analyze_escape, EscapeState};
use optimize_js::TopLevelMode;

fn escape_states_for_first_function(source: &str) -> Vec<EscapeState> {
  let program = compile_source(source, TopLevelMode::Module, false);
  assert_eq!(
    program.functions.len(),
    1,
    "expected exactly one nested function"
  );
  let results = analyze_escape(&program.functions[0].body);
  results.iter().map(|(_, state)| state).collect()
}

#[test]
fn alloc_stored_into_param_property_is_arg_escape() {
  let states = escape_states_for_first_function(
    r#"
      const f = (p) => {
        const a = {};
        p.x = a;
      };
    "#,
  );

  assert_eq!(states.len(), 1);
  assert!(
    matches!(states[0], EscapeState::ArgEscape(_)),
    "expected ArgEscape, got {states:?}"
  );
}

#[test]
fn alloc_returned_is_return_escape() {
  let states = escape_states_for_first_function(
    r#"
      const f = () => {
        const a = {};
        return a;
      };
    "#,
  );

  assert_eq!(states.len(), 1);
  assert_eq!(states[0], EscapeState::ReturnEscape);
}

#[test]
fn containment_propagates_return_escape() {
  let states = escape_states_for_first_function(
    r#"
      const f = () => {
        const a = {};
        const b = { x: a };
        return b;
      };
    "#,
  );

  assert_eq!(states.len(), 2);
  assert!(
    states.iter().all(|s| *s == EscapeState::ReturnEscape),
    "expected both allocations to be ReturnEscape, got {states:?}"
  );
}

#[test]
fn non_escaping_allocations_are_no_escape() {
  let states = escape_states_for_first_function(
    r#"
      const f = () => {
        const a = {};
        const b = { x: a };
        b.y = 1;
      };
    "#,
  );

  assert_eq!(states.len(), 2);
  assert!(
    states.iter().all(|s| *s == EscapeState::NoEscape),
    "expected both allocations to be NoEscape, got {states:?}"
  );
}
