#![allow(unused_imports)]

// This test is intentionally tiny and mostly compile-time:
// it ensures FastRender's pinned `vm-js` (via the ecma-rs submodule) keeps exposing
// the embedding/budget APIs we rely on.

use vm_js::docs::webidl_host_objects;
use vm_js::{Budget, Vm, VmOptions};

#[test]
fn vm_js_budget_api_is_available() {
  let mut vm = Vm::new(VmOptions::default());

  // API surface: Task budgets + restoration helpers.
  vm.reset_budget_to_default();
  let _guard = vm.push_budget(Budget {
    fuel: Some(1),
    deadline: None,
    check_time_every: 1,
  });
}
