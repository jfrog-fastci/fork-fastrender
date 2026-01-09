# `lint-no-openssl` (OpenSSL build dependency regression gate)

FastRender aims to build in **hermetic** environments (CI/agents/containers) without requiring
system-installed OpenSSL development packages (`libssl-dev`, `openssl-devel`, etc).

Historically, the easiest way for OpenSSL to creep back in is via HTTP client default features
(notably `reqwest` pulling `native-tls` → `openssl-sys`). To prevent regressions, we maintain an
`xtask` lint that fails CI if **any** build graph includes `openssl-sys`.

## Running

```bash
# Recommended (matches CI):
bash scripts/cargo_agent.sh xtask lint-no-openssl --workspace --all-features
```

This runs `cargo metadata --locked` and inspects the resolved dependency graph.

## Scope

- Default (no flag): traverses dependencies starting from the `fastrender` crate.
- `--workspace`: traverses dependencies starting from **all workspace members**.
  - CI runs workspace-wide because `cargo test --all-features` at the workspace root compiles
    everything, not just the core renderer crate.

## Feature sets

- Default: checks the default feature set.
- `--all-features`: also checks the `--all-features` graph (recommended).

## Debugging failures

When `openssl-sys` is found, the lint prints the dependency chain(s) that pulled it in
(`name@version -> ... -> openssl-sys@version`).

For deeper inspection, `cargo tree` is usually enough:

```bash
# Show all reverse-dependency paths that reach openssl-sys:
cargo tree -i openssl-sys
```

In most cases, the fix is to switch the offending crate to a Rust TLS backend (e.g. `rustls`)
or disable `default-features` and explicitly select the non-OpenSSL feature set.
