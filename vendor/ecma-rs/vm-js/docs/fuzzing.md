# Fuzzing (`vm-js` + `parse-js`)

This repo uses **libFuzzer via `cargo-fuzz`** to continuously shake out:

- parser panics/crashes in `parse-js`
- VM panics/crashes/invariant violations in `vm-js`
- missing budget checks (hangs) and heap-accounting bugs (OOM safety)

The fuzz targets live in `vendor/ecma-rs/fuzz/`:

- `parse_js` (parser-only)
- `vm_js_exec` (parse + execute via `vm-js::Agent::run_script`)

## Safety: budgets + heap limits

The `vm_js_exec` target is intentionally defensive:

- **Fuel budget:** 10k ticks per run
- **Deadline:** ~20ms per run
- **Heap limit:** 16MiB max (GC threshold 8MiB)
- **Interrupt flag:** enabled; sometimes pre-set based on input to exercise interruption paths

This ensures inputs like `while(true){}` terminate quickly under normal operation.

Note that budgets/interrupts are **cooperative** and rely on the evaluator calling `Vm::tick()`.
If a bug bypasses ticking, a fuzz run can still hang. For that reason, always run fuzzers under an
**OS-level timeout** (see below).

## Running (agent-safe)

From the repo root:

```bash
cd vendor/ecma-rs/fuzz

# Always wrap with `timeout` so a missed tick can't hang the agent indefinitely.
timeout 30s cargo fuzz run parse_js -- ../vm-js/fuzz/corpus/parse_js
timeout 30s cargo fuzz run vm_js_exec -- ../vm-js/fuzz/corpus/vm_js_exec
```

Tip: add `-- -max_len=8192` to cap libFuzzer's generated input size (the harness also caps).

## Regression workflow (crash → minimized seed → unit test)

When `cargo fuzz` finds a crash it writes an artifact under:

- `vendor/ecma-rs/fuzz/artifacts/<target>/...` (gitignored)

Recommended workflow:

1. **Minimize** the crashing input:

   ```bash
   cd vendor/ecma-rs/fuzz
   cargo fuzz tmin vm_js_exec artifacts/vm_js_exec/crash-... > /tmp/vm-js.min
   ```

2. **Commit the minimized seed** into the tracked regression corpus:

   ```bash
   cp /tmp/vm-js.min ../vm-js/fuzz/corpus/vm_js_exec/<short_name>.js
   ```

3. **Add a unit test reproducer** under `vendor/ecma-rs/vm-js/tests/` using `include_str!`:

   ```rust
   let src = include_str!("../fuzz/corpus/vm_js_exec/<short_name>.js");
   // run with a tiny budget/heap; assert it does not panic
   ```

Keeping the seed + test in-tree ensures the bug stays fixed even if the fuzzer corpus is pruned.

