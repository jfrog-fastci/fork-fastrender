# optimize-js-debugger

Developer UI for inspecting `optimize-js` optimizer output (CFG, debug steps, symbols).

## Program dump schema

`POST /compile` returns a MessagePack-encoded, versioned `ProgramDump` payload:

```json
{ "version": "v1", "program": { /* ProgramDumpV1 */ } }
```

Each instruction in the CFG/debug steps may include an optional `meta` field (effects/purity/escape/ownership/type/layout).

`POST /compile_dump` returns the newer `optimize_js::dump::ProgramDump` schema (with full analysis metadata).

## Quickstart

### Install UI dependencies

```bash
cd optimize-js-debugger
npm install
```

### Run the Rust server

From the repository root:

```bash
bash scripts/cargo_agent.sh run -p optimize-js-debugger
```

This starts an HTTP server on `http://localhost:3001`.

Endpoints:

- `POST /compile_dump`: returns the new `optimize_js::dump::ProgramDump` schema (includes per-instruction
  analysis metadata). This is what the UI uses for the \"Analyzed CFG\" view.
  - Request flags (all optional): `typed`, `semantic_ops`, `run_analyses`.
- `POST /compile`: versioned snapshot schema used by the UI for optimizer debug steps and symbols.

### Run the UI dev server

In another terminal:

```bash
cd optimize-js-debugger
npm start
```

Then open the URL printed by esbuild (typically `http://localhost:8000`).

### Run UI tests / production build

```bash
cd optimize-js-debugger
npm test
npm run build
```

Build output is written to `optimize-js-debugger/dist/`.

## Snapshot mode (Rust CLI)

The Rust binary can also emit a JSON snapshot of the optimizer output instead of
starting the HTTP server:

```bash
bash scripts/cargo_agent.sh run -p optimize-js-debugger -- \
  --snapshot \
  --input optimize-js-debugger/tests/fixtures/debug_input.js \
  --mode module \
  --output /tmp/optimize-js-debugger.snapshot.json
```

Flags:

- `--snapshot`: enable snapshot mode
- `--input <path>`: read JS source from a file (default: stdin)
- `--output <path>`: write JSON to a file (default: stdout)
- `--mode <global|module>`: top-level mode (default: `module`)
