# Planner scratchpad (repo hygiene)

This file is checked into the repository so future planning doesn't trip over
already-completed work. (Agent-local `scratchpad.md` files are ignored by git in
this swarm environment.)

## Repo health (ecma-rs)

Last checked: 2026-01-14 @ `b13f7faa`

- `timeout -k 10 600 bash vendor/ecma-rs/scripts/cargo_agent.sh test -p parse-js --lib` — **FAIL** (2 tests)
  - `parse::tests::class_field_initializer::arguments_identifier_reference_is_allowed_in_function_in_class_field_initializer`
    - `parse-js/src/parse/tests/class_field_initializer.rs:52:3`
    - `parse failed: Err(ExpectedSyntax("`arguments` is not allowed in class field initializers or static initialization blocks") ... loc [39:48])`
  - `parse::tests::class_static_block::arguments_identifier_reference_is_allowed_in_function_in_static_block`
    - `parse-js/src/parse/tests/class_static_block.rs:246:3`
    - `parse failed: Err(ExpectedSyntax("`arguments` is not allowed in class field initializers or static initialization blocks") ... loc [53:62])`

- `timeout -k 10 600 bash vendor/ecma-rs/scripts/cargo_agent.sh test -p vm-js --test early_errors` — **FAIL** (1 test)
  - `await_using_declaration_in_script_block_is_syntax_error`
    - `vm-js/tests/early_errors.rs:1556:57`
    - `called Result::unwrap_err() on an Ok value`

- `timeout -k 10 600 bash vendor/ecma-rs/scripts/cargo_agent.sh test -p vm-js --lib` — **FAIL** (compile)
  - `error[E0061]: SourceTextModuleRecord::parse_source` now requires `heap: &mut Heap`
    - `vm-js/tests/compiled_module_graph.rs:2711:24`
    - `vm-js/tests/compiled_module_graph.rs:2828:24`

## Open tasks

- Fix `parse-js --lib` failures (likely: `arguments` check should not traverse into nested function bodies):
  - `parse::tests::class_field_initializer::arguments_identifier_reference_is_allowed_in_function_in_class_field_initializer`
  - `parse::tests::class_static_block::arguments_identifier_reference_is_allowed_in_function_in_static_block`

- Fix `vm-js --test early_errors` failure:
  - `await_using_declaration_in_script_block_is_syntax_error` (`vm-js/tests/early_errors.rs:1556:57` — `unwrap_err` called on `Ok`)

- Fix `vm-js --lib` compile error after `SourceTextModuleRecord::parse_source` signature change:
  - `vm-js/tests/compiled_module_graph.rs:2711:24` and `:2828:24` call `parse_source(...)` without `heap: &mut Heap`

## Completed

- [x] Task 328 — UTF-8 API guard (`bash scripts/check_utf8_apis.sh` exits 0)
- [x] Task 338 — `docs/deps.md` regeneration is clean (`bash scripts/gen_deps_graph.sh` produces no diff)
- [x] Task 340 — Diagnostic code `NATIVE0001` is valid (`bash scripts/check_diagnostic_codes.sh` exits 0)
