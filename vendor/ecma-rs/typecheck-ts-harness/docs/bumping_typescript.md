# Bumping the pinned TypeScript version

This repo intentionally pins a single TypeScript version and keeps **four**
things in sync:

1. **Rust bundled lib `.d.ts` files** (used by `typecheck-ts` when built with the
    `bundled-libs` feature)
    - `typecheck-ts/build.rs` (`TYPESCRIPT_VERSION`)
    - `typecheck-ts/fixtures/typescript-libs/<ver>/…`
2. **Rust resolver semver default** (used for `typesVersions` selection when the
   host doesn’t specify a compiler version)
   - `typecheck-ts/src/resolve/ts_node.rs` (`TypeScriptVersion::default()`)
3. **Node.js oracle (`tsc`) used by the harness**
    - `typecheck-ts-harness/package.json`
    - `typecheck-ts-harness/package-lock.json`
4. **Baselines/snapshots generated from that exact `tsc`**
    - `typecheck-ts-harness/baselines/**`

CI runs `scripts/check_typescript_version_sync.sh` to prevent drift between (1),
(2) and (3). CI also runs `typecheck-ts-harness lint-baselines` to ensure
baselines match the pinned `tsc` version.

## Procedure (checklist)

### 0) Pick a target version

Decide the exact version string (example: `5.10.0`). We pin **exact** versions
(no `^`/`~` ranges).

#### Optional: use the bump helper script

For the mechanical edits + vendoring, you can run (from `vendor/ecma-rs/`):

```bash
./scripts/bump_typescript_version.sh <newver>
```

That script updates `build.rs`, the harness npm manifests, and vendors the
`lib*.d.ts` files into `typecheck-ts/fixtures/typescript-libs/<newver>/`. It also
updates the resolver’s default `TypeScriptVersion` used for `typesVersions`
selection (`typecheck-ts/src/resolve/ts_node.rs`).
You still need to regenerate baselines/snapshots (steps 5–6 below).

### 1) Update the Rust bundled version

Edit `typecheck-ts/build.rs`:

- Update `const TYPESCRIPT_VERSION: &str = "<newver>";`
- Update `typecheck-ts/src/resolve/ts_node.rs` (`TypeScriptVersion::default()`) to match `<newver>`

### 2) Vendor the upstream TypeScript lib `.d.ts` files

Create a new directory:

- `typecheck-ts/fixtures/typescript-libs/<newver>/`

Populate it with the upstream TypeScript libs from the `typescript` npm package:

- Copy `typescript/lib/lib*.d.ts` into that directory
- Also copy the license files for auditing:
  - `LICENSE.txt` (TypeScript, Apache 2.0)
  - `ThirdPartyNoticeText.txt`
- Add/update `README.md` in that directory (see the existing `5.x.y/README.md` for the expected format).

Notes:

- `typecheck-ts/build.rs` only embeds files matching `lib.*.d.ts`, so those must
  exist in the vendored directory.
- It’s OK (and expected) for the repo to contain multiple version directories
  during a transition, but CI and the Rust build will only use the one matching
  `TYPESCRIPT_VERSION`.

### 3) Update the Node harness pinned `typescript` version

Edit:

- `typecheck-ts-harness/package.json` → set `"typescript": "<newver>"`

Then update the lockfile:

```bash
cd typecheck-ts-harness
npm install --ignore-scripts
```

Commit both `package.json` and `package-lock.json`.

### 4) Check version sync (must pass)

From `vendor/ecma-rs/`:

```bash
./scripts/check_typescript_version_sync.sh
```

If this fails, do **not** proceed to regenerating baselines; fix the mismatch
first.

### 5) Regenerate difftsc baselines

`difftsc` baselines are stored under `typecheck-ts-harness/baselines/difftsc/`.

Ensure the Node dependencies are installed:

```bash
cd typecheck-ts-harness
npm ci --ignore-scripts
cd ..
```

Then regenerate baselines:

```bash
bash scripts/cargo_agent.sh run -p typecheck-ts-harness --release -- \
  difftsc \
  --suite typecheck-ts-harness/fixtures/difftsc \
  --update-baselines \
  --manifest typecheck-ts-harness/fixtures/difftsc/manifest.toml
```

Note: CI runs `typecheck-ts-harness lint-baselines` which checks that committed
baseline JSON includes `metadata.typescript_version` matching the pinned version
from `typecheck-ts-harness/package-lock.json` (specifically
`packages["node_modules/typescript"].version`). If you bump TypeScript but forget
to regenerate baselines, CI will fail with a mismatch error.

### 6) Verify (and/or update) conformance snapshots

Snapshot integrity checks compare *stored snapshots* against *live tsc* for the
same discovered tests.

Run:

```bash
bash scripts/cargo_agent.sh run -p typecheck-ts-harness --release -- \
  verify-snapshots \
  --root typecheck-ts-harness/fixtures/conformance-mini \
  --jobs 2 \
  --timeout-secs 20
```

If snapshots drift because the new TypeScript version changes diagnostics/spans,
update them by rerunning `conformance` with `--update-snapshots`, then re-run
`verify-snapshots`:

```bash
bash scripts/cargo_agent.sh run -p typecheck-ts-harness --release -- \
  conformance \
  --root typecheck-ts-harness/fixtures/conformance-mini \
  --compare snapshot \
  --update-snapshots
```

### 7) CI hygiene

Historically, CI sometimes had a duplicated `TYPESCRIPT_VERSION` environment
variable in workflow YAML. Prefer a single source of truth (the pinned harness
lockfile + the Rust `TYPESCRIPT_VERSION`) and keep them consistent via
`scripts/check_typescript_version_sync.sh`.

After bumping, ensure CI still runs:

- `difftsc --use-baselines` against the updated baselines
- the live `tsc` job (installs from `typecheck-ts-harness/package-lock.json`)
