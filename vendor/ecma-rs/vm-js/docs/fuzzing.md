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
# Install `cargo-fuzz` (one-time).
timeout -k 10 600 bash scripts/cargo_agent.sh install cargo-fuzz

# Create gitignored output corpus directories (one-time).
mkdir -p vendor/ecma-rs/fuzz/corpus/parse_js vendor/ecma-rs/fuzz/corpus/vm_js_exec

# Always wrap with `timeout -k` so a missed tick can't hang the agent indefinitely.
#
# Use the repo's cargo wrapper: it bumps RLIMIT_AS for fuzz runs (ASan shadow memory),
# and it automatically runs Cargo from the right workspace root.
#
# IMPORTANT: the **first** corpus directory passed to libFuzzer is treated as the output corpus
# (new inputs are written into it). Use the gitignored `vendor/ecma-rs/fuzz/corpus/...` as the
# output corpus, and pass the tracked regression seeds (`vm-js/fuzz/corpus/...`) as an additional
# read-only corpus.
timeout -k 10 600 bash vendor/ecma-rs/scripts/cargo_agent.sh fuzz run parse_js fuzz/corpus/parse_js vm-js/fuzz/corpus/parse_js -- -max_total_time=10
timeout -k 10 600 bash vendor/ecma-rs/scripts/cargo_agent.sh fuzz run vm_js_exec fuzz/corpus/vm_js_exec vm-js/fuzz/corpus/vm_js_exec -- -max_total_time=10
```

Tip: add `-- -max_len=8192` to cap libFuzzer's generated input size (the harness also caps).

## Regression workflow (crash → minimized seed → unit test)

When `cargo fuzz` finds a crash it writes an artifact under:

- `vendor/ecma-rs/fuzz/artifacts/<target>/...` (gitignored)

Recommended workflow:

1. **Minimize** the crashing input:

   ```bash
   # `cargo fuzz tmin` runs the target to minimize it; wrap with timeout.
   timeout -k 10 600 bash vendor/ecma-rs/scripts/cargo_agent.sh fuzz tmin vm_js_exec fuzz/artifacts/vm_js_exec/crash-... > /tmp/vm-js.min
   ```

2. **Commit the minimized seed** into the tracked regression corpus:

   ```bash
   cp /tmp/vm-js.min vendor/ecma-rs/vm-js/fuzz/corpus/vm_js_exec/<short_name>.js
   ```

3. **Add a unit test reproducer** under `vendor/ecma-rs/vm-js/tests/` using `include_str!`:

   ```rust
   let src = include_str!("../fuzz/corpus/vm_js_exec/<short_name>.js");
   // run with a tiny budget/heap; assert it does not panic
   ```

Keeping the seed + test in-tree ensures the bug stays fixed even if the fuzzer corpus is pruned.
