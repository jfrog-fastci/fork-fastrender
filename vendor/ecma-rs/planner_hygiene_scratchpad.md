# Planner scratchpad (repo hygiene)

This file is checked into the repository so future planning doesn't trip over
already-completed work. (Agent-local `scratchpad.md` files are ignored by git in
this swarm environment.)

## Repo health (ecma-rs)

Last checked: 2026-01-14

- `timeout -k 10 600 bash vendor/ecma-rs/scripts/cargo_agent.sh test -p parse-js --lib` — **PASS** (`149` tests)

- `timeout -k 10 600 bash vendor/ecma-rs/scripts/cargo_agent.sh test -p vm-js --test early_errors` — **PASS** (`199` tests)

- `timeout -k 10 600 bash vendor/ecma-rs/scripts/cargo_agent.sh test -p vm-js --lib` — **FAIL** (`14` tests)
  - `compiled_module_graph_tests::compiled_module_export_default_expr_applies_set_function_name_default`
    - `vm-js/tests/compiled_module_graph.rs:2090` (`assertion left == right failed; left: Rejected, right: Fulfilled`)
  - `compiled_module_graph_tests::compiled_module_export_default_async_arrow_expr_applies_set_function_name_default`
    - `vm-js/tests/compiled_module_graph.rs:2187` (`assertion left == right failed: module evaluation should complete synchronously; left: Rejected, right: Fulfilled`)
  - `compiled_module_graph_tests::compiled_module_export_default_async_function_expr_applies_set_function_name_default`
    - `vm-js/tests/compiled_module_graph.rs:2296` (`assertion left == right failed: module evaluation should complete synchronously; left: Rejected, right: Fulfilled`)
  - `compiled_module_graph_tests::compiled_module_export_default_class_expr_constructs_and_applies_set_function_name_default`
    - `vm-js/tests/compiled_module_graph.rs:2409` (`assertion left == right failed; left: Rejected, right: Fulfilled`)
  - `compiled_module_graph_tests::compiled_module_export_default_class_expr_static_name_method_can_override_and_internal_name_is_default`
    - `vm-js/tests/compiled_module_graph.rs:2511` (`assertion left == right failed; left: Rejected, right: Fulfilled`)
  - `compiled_module_graph_tests::compiled_module_export_default_function_expr_applies_set_function_name_default`
    - `vm-js/tests/compiled_module_graph.rs:2627` (`assertion left == right failed; left: Rejected, right: Fulfilled`)
  - `compiled_module_graph_tests::compiled_module_export_default_class_expr_applies_set_function_name_default`
    - `vm-js/tests/compiled_module_graph.rs:2731` (`assertion left == right failed; left: Rejected, right: Fulfilled`)
  - `compiled_module_graph_tests::compiled_module_export_default_expr_respects_statement_order`
    - `vm-js/tests/compiled_module_graph.rs:1987` (`assertion left == right failed; left: Rejected, right: Fulfilled`)
  - `compiled_module_graph_tests::compiled_module_graph_default_export_expression_is_evaluated_once`
    - `ThrowWithStack { source: "a.js", line: 1, col: 1 }`
  - `hir_exec::async_for_await_of_async_iterator_close_tests::compiled_hir_async_fn_for_await_of_throw_awaits_async_iterator_close_before_catch`
    - `vm-js/src/hir_exec.rs` (`InvariantViolation("for-await-of executed in synchronous HIR evaluator")`)
  - `object_literal_super_tests::super_computed_getsuperbase_before_topropertykey_getvalue`
    - `vm-js/tests/unit/object_literal_super.rs:64` (`unexpected interpreter result: Bool(false)`)
  - `object_literal_super_tests::super_computed_getsuperbase_before_topropertykey_putvalue`
    - `vm-js/tests/unit/object_literal_super.rs:64` (`unexpected interpreter result: Bool(false)`)
  - `object_literal_super_tests::super_computed_getsuperbase_before_topropertykey_putvalue_compound_assign`
    - `vm-js/tests/unit/object_literal_super.rs:64` (`unexpected interpreter result: Bool(false)`)
  - `object_literal_super_tests::super_computed_getsuperbase_before_topropertykey_putvalue_increment`
    - `vm-js/tests/unit/object_literal_super.rs:64` (`unexpected interpreter result: Bool(false)`)

## Open tasks

- Fix `vm-js --lib` failures:
  - Compiled module graph default-export evaluation is rejecting unexpectedly:
    - `compiled_module_graph_tests::compiled_module_export_default_expr_applies_set_function_name_default`
      (`left: Rejected, right: Fulfilled`)
    - `compiled_module_graph_tests::compiled_module_export_default_async_arrow_expr_applies_set_function_name_default`
      (`left: Rejected, right: Fulfilled`)
    - `compiled_module_graph_tests::compiled_module_export_default_async_function_expr_applies_set_function_name_default`
      (`left: Rejected, right: Fulfilled`)
    - `compiled_module_graph_tests::compiled_module_export_default_class_expr_constructs_and_applies_set_function_name_default`
      (`left: Rejected, right: Fulfilled`)
    - `compiled_module_graph_tests::compiled_module_export_default_class_expr_static_name_method_can_override_and_internal_name_is_default`
      (`left: Rejected, right: Fulfilled`)
    - `compiled_module_graph_tests::compiled_module_export_default_function_expr_applies_set_function_name_default`
      (`left: Rejected, right: Fulfilled`)
    - `compiled_module_graph_tests::compiled_module_export_default_class_expr_applies_set_function_name_default`
      (`left: Rejected, right: Fulfilled`)
    - `compiled_module_graph_tests::compiled_module_export_default_expr_respects_statement_order`
      (`left: Rejected, right: Fulfilled`)
    - `compiled_module_graph_tests::compiled_module_graph_default_export_expression_is_evaluated_once`
      (`ThrowWithStack { source: "a.js", line: 1, col: 1 }`)
  - Async `for await..of` in compiled HIR:
    - `hir_exec::async_for_await_of_async_iterator_close_tests::compiled_hir_async_fn_for_await_of_throw_awaits_async_iterator_close_before_catch`
      (`InvariantViolation("for-await-of executed in synchronous HIR evaluator")`)
  - `super` in object literal computed properties produces incorrect side-effect ordering:
    - `object_literal_super_tests::super_computed_getsuperbase_before_topropertykey_getvalue`
      (`unexpected interpreter result: Bool(false)`)
    - `object_literal_super_tests::super_computed_getsuperbase_before_topropertykey_putvalue`
      (`unexpected interpreter result: Bool(false)`)
    - `object_literal_super_tests::super_computed_getsuperbase_before_topropertykey_putvalue_compound_assign`
      (`unexpected interpreter result: Bool(false)`)
    - `object_literal_super_tests::super_computed_getsuperbase_before_topropertykey_putvalue_increment`
      (`unexpected interpreter result: Bool(false)`)

## Completed

- [x] Task 328 — UTF-8 API guard (`bash scripts/check_utf8_apis.sh` exits 0)
- [x] Task 338 — `docs/deps.md` regeneration is clean (`bash scripts/gen_deps_graph.sh` produces no diff)
- [x] Task 340 — Diagnostic code `NATIVE0001` is valid (`bash scripts/check_diagnostic_codes.sh` exits 0)
