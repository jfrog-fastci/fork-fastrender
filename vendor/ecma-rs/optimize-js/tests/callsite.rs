#[path = "common/mod.rs"]
mod common;

use common::compile_source;
use optimize_js::analysis::callsite::{analyze_callsites, CallSiteCallee};
use optimize_js::il::inst::{Arg, Const};
use optimize_js::TopLevelMode;

#[test]
fn callsite_recognizes_member_call() {
  let program = compile_source("obj.m(1);", TopLevelMode::Module, false);
  let callsites = analyze_callsites(&program.top_level.body);

  assert!(
    callsites.values().any(|info| matches!(
      &info.callee,
      CallSiteCallee::Member {
        property: Arg::Const(Const::Str(prop)),
        ..
      } if prop == "m"
    )),
    "expected a member call to property `m`, got: {callsites:?}"
  );
}

#[test]
fn callsite_direct_builtin_is_recorded() {
  // Ensure the call stays non-const-evaluable so the `Call` instruction remains after DVN.
  let program = compile_source("Math.floor(x);", TopLevelMode::Module, false);
  let callsites = analyze_callsites(&program.top_level.body);

  assert!(
    callsites.values().any(|info| matches!(
      &info.callee,
      CallSiteCallee::DirectBuiltin(path) if path == "Math.floor"
    )),
    "expected a direct builtin call to `Math.floor`, got: {callsites:?}"
  );
}

