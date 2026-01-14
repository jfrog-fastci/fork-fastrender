# Planner scratchpad (repo hygiene)

This file is checked into the repository so future planning doesn't trip over
already-completed work. (Agent-local `scratchpad.md` files are ignored by git in
this swarm environment.)

## Repo health (ecma-rs)

Last checked: 2026-01-14

- `timeout -k 10 600 bash vendor/ecma-rs/scripts/cargo_agent.sh test -p parse-js --lib` — **PASS** (`115` tests)

- `timeout -k 10 600 bash vendor/ecma-rs/scripts/cargo_agent.sh test -p vm-js --test early_errors` — **FAIL** (1 test)
  - `await_using_declaration_in_script_block_is_syntax_error`
    - `vm-js/tests/early_errors.rs:1556:57`
    - `called Result::unwrap_err() on an Ok value`

- `timeout -k 10 600 bash vendor/ecma-rs/scripts/cargo_agent.sh test -p vm-js --lib` — **FAIL** (`11` tests)
  - `compiled_module_execution_context_tests::compiled_module_import_meta_uses_callee_module_and_is_cached`
    - `vm-js/tests/compiled_module_execution_context.rs:320:7` (`Rejected` vs `Fulfilled`)
  - `compiled_module_execution_context_tests::compiled_module_dynamic_import_referrer_uses_callee_module`
    - `Error: Unimplemented(\"unbound identifier\")`
  - `compiled_module_decl_execution_context_tests::compiled_module_decl_functions_capture_realm_and_module_for_host_calls`
    - `vm-js/tests/compiled_module_decl_execution_context.rs:371:7` (`Rejected` vs `Fulfilled`)
  - `compiled_module_graph_tests::compiled_module_graph_dynamic_import_from_compiled_module_resolves`
    - `Error: ThrowWithStack { ... }`
  - `compiled_module_graph_tests::compiled_module_graph_import_meta_is_cached_within_compiled_module`
    - `Error: ThrowWithStack { ... }`
  - `compiled_module_graph_tests::compiled_module_hoists_top_level_function_decls_into_module_env`
    - `vm-js/tests/compiled_module_graph.rs:2983:5` (`Rejected` vs `Fulfilled`)
  - `compiled_module_graph_tests::compiled_module_supports_named_default_export_function_decls`
    - `vm-js/tests/compiled_module_graph.rs:3073:5` (`Rejected` vs `Fulfilled`)
  - `hir_exec::hir_async_await_control_flow_regression_tests::labelled_continue_across_await`
    - `Error: InvariantViolation(\"PromiseReactionJob handler threw while capability is undefined\")`
  - `typed_array_dataview_rooting_gc_tests::function_bind_roots_bound_args_across_gc_in_length_get_trap`
    - panic at `vm-js/src/heap.rs:9376:7` (`debug_value_is_valid_or_primitive`)
  - `typed_array_dataview_rooting_gc_tests::array_pop_roots_result_across_gc_in_length_set_trap`
    - panic at `vm-js/src/heap.rs:9376:7` (`debug_value_is_valid_or_primitive`)
  - `typed_array_dataview_rooting_gc_tests::reflect_apply_roots_target_across_gc_in_create_list_from_array_like`
    - panic at `vm-js/src/heap.rs:9376:7` (`debug_value_is_valid_or_primitive`)

## Open tasks

- Fix `vm-js --test early_errors` failure:
  - `await_using_declaration_in_script_block_is_syntax_error` (`vm-js/tests/early_errors.rs:1556:57` — `unwrap_err` called on `Ok`)

- Fix `vm-js --lib` failures (see failing tests list above).

## Completed

- [x] Task 328 — UTF-8 API guard (`bash scripts/check_utf8_apis.sh` exits 0)
- [x] Task 338 — `docs/deps.md` regeneration is clean (`bash scripts/gen_deps_graph.sh` produces no diff)
- [x] Task 340 — Diagnostic code `NATIVE0001` is valid (`bash scripts/check_diagnostic_codes.sh` exits 0)
