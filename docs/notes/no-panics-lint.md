# `lint-no-panics` (panic/unwrap regression gate)

FastRender has a non-negotiable invariant: **no panics in production code** (see `AGENTS.md`).

To make this harder to regress accidentally (e.g. via `unwrap()`/`expect()`), we maintain a small
project lint that scans the Rust sources and fails CI if *new* panic sites are introduced.

## Running

```bash
cargo xtask lint-no-panics
```

This scans `src/` and reports any new occurrences of:

- `panic!(` / `todo!(` / `unimplemented!(`
- `assert!(` / `assert_eq!(` / `assert_ne!(` / `unreachable!(`
- `.unwrap(` / `.expect(`

The scan ignores:

- `#[cfg(test)]` items/blocks
- `tests/` integration test files (not under `src/` anyway)

Note: `debug_assert!` (and friends) are intentionally **not** flagged because they are compiled out
in release builds.

## Baseline file (existing violations)

The repository currently contains some legacy panic sites. These are tracked in:

- `tools/no_panics_baseline.json`

`cargo xtask lint-no-panics` fails only when **new** violations appear beyond that baseline.

After you remove existing violations, regenerate the baseline and commit the updated file:

```bash
cargo xtask lint-no-panics --update-baseline
```

## Audited exceptions (use sparingly)

If a specific panic site is truly unavoidable and has been carefully audited, it can be excluded
by adding one of these markers on the same line:

- `// fastrender-allow-panic`
- `// fastrender-allow-unwrap`

Prefer refactoring to avoid the panic site whenever possible.
