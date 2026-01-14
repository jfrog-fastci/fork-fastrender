# Planner scratchpad (repo hygiene)

This file is checked into the repository so future planning doesn't trip over
already-completed work. (Agent-local `scratchpad.md` files are ignored by git in
this swarm environment.)

## Repo health (ecma-rs)

Last checked: 2026-01-14

- `timeout -k 10 600 bash vendor/ecma-rs/scripts/cargo_agent.sh test -p parse-js --lib` — **PASS** (`161` tests)

- `timeout -k 10 600 bash vendor/ecma-rs/scripts/cargo_agent.sh test -p vm-js --test early_errors` — **PASS** (`236` tests)

- `timeout -k 10 600 bash vendor/ecma-rs/scripts/cargo_agent.sh test -p vm-js --lib` — **FAIL** (`14` failing / `866` tests)

## Open tasks

- Fix failing `vm-js --lib` tests:
  - `compiled_module_graph_tests::compiled_module_rejection_error_object_has_throw_site_stack` — `Error: PropertyNotData`
  - `exec::tests::arrow_this_in_derived_constructor_delete_super_property_throws_and_preserves_uninitialized_this_error` — assertion failed (`left: Bool(false)`, `right: Bool(true)`) at `vm-js/src/exec.rs:64174`
  - `exec::tests::logical_assignment_anonymous_function_name_inference` — assertion failed (`left: Bool(false)`, `right: Bool(true)`) at `vm-js/src/exec.rs:63050`
  - `hir_exec::hir_async_await_stack_tests::hir_async_await_rejection_error_stack_attributed_to_await_site` — `Error: Unimplemented("Heap::get accessor properties require a VM to call getters")`
  - `hir_exec::hir_async_await_stack_tests::top_level_await_rejection_error_stack_attributed_to_await_site` — `Error: Unimplemented("Heap::get accessor properties require a VM to call getters")`
  - `hir_exec::hir_async_await_stack_tests::top_level_for_await_of_rhs_await_rejection_error_stack_attributed_to_await_site` — `Error: Unimplemented("Heap::get accessor properties require a VM to call getters")`
  - `hir_exec::hir_async_await_stack_tests::top_level_for_triple_multi_await_rejection_error_stack_attributed_to_latest_await_site` — `Error: Unimplemented("Heap::get accessor properties require a VM to call getters")`
  - `hir_exec::hir_async_loop_rhs_await_regression_tests::compiled_async_for_await_of_with_await_in_rhs_and_body` — expected compiled HIR async evaluator, got `AsyncEcmaFallback { code_id: EcmaFunctionId(0) }` (panic at `vm-js/src/hir_exec.rs:26952`)
  - `hir_exec::hir_async_loop_rhs_await_regression_tests::compiled_async_for_in_with_await_in_rhs` — expected compiled HIR async evaluator, got `AsyncEcmaFallback { code_id: EcmaFunctionId(0) }` (panic at `vm-js/src/hir_exec.rs:26952`)
  - `hir_exec::hir_async_loop_rhs_await_regression_tests::compiled_async_for_of_with_await_in_rhs` — expected compiled HIR async evaluator, got `AsyncEcmaFallback { code_id: EcmaFunctionId(0) }` (panic at `vm-js/src/hir_exec.rs:26952`)
  - `home_object_tests::await_in_class_static_block_is_syntax_error_ast` — `Error: Syntax(VMJS0004: await is not allowed in class static initialization block)`
  - `home_object_tests::await_in_class_static_block_is_syntax_error_module` — `Error: Syntax(VMJS0004: await is not allowed in class static initialization block)`
  - `logical_assignment_tests::logical_assignment_anonymous_function_name_inference` — assertion failed (`left: Bool(false)`, `right: Bool(true)`) at `vm-js/tests/logical_assignment.rs:154`
  - `vm::tests::intrinsics_do_not_register_duplicate_native_calls` — assertion failed (`left: 3`, `right: 1`) at `vm-js/src/vm.rs:5299`

## Completed

- [x] Task 328 — UTF-8 API guard (`bash scripts/check_utf8_apis.sh` exits 0)
- [x] Task 338 — `docs/deps.md` regeneration is clean (`bash scripts/gen_deps_graph.sh` produces no diff)
- [x] Task 340 — Diagnostic code `NATIVE0001` is valid (`bash scripts/check_diagnostic_codes.sh` exits 0)
