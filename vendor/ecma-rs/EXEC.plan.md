# EXEC.plan.md (compatibility shim)

This repository historically used `EXEC.plan.md` as the canonical implementation plan for the
native AOT compilation track. The plan has since been split by workstream:

- **Repo-wide agent rules / resource limits:** [`AGENTS.md`](./AGENTS.md)
- **TypeScript type checking workstream:** [`instructions/ts_typecheck.md`](./instructions/ts_typecheck.md)
- **Native AOT compilation workstream (LLVM):** [`instructions/native_aot.md`](./instructions/native_aot.md)

This file is intentionally kept as a **stable permalink** for older docs, code comments, tools, and
agent instructions that still link to `EXEC.plan.md`.

## Section references

Many documents refer to numbered sections like “§5.5 Async Runtime”. Those sections live in
[`instructions/native_aot.md`](./instructions/native_aot.md) (the content originally found in this file).

Common entry points:

- System requirements: [`instructions/native_aot.md#system-requirements-ubuntu-x64`](./instructions/native_aot.md#system-requirements-ubuntu-x64)
- Strict TypeScript dialect: [`instructions/native_aot.md#our-typescript-dialect-strict-mode`](./instructions/native_aot.md#our-typescript-dialect-strict-mode)
- Async runtime plan: [`instructions/native_aot.md#55-async-runtime`](./instructions/native_aot.md#55-async-runtime)

---

## Anchor compatibility stubs

Some older links point directly at `EXEC.plan.md#...` section anchors. To keep those links working,
we provide a small set of stub headings that redirect to the canonical docs.

## System Requirements (Ubuntu x64)

Moved to [`instructions/native_aot.md#system-requirements-ubuntu-x64`](./instructions/native_aot.md#system-requirements-ubuntu-x64).

## Our TypeScript Dialect ("Strict Mode")

Moved to [`instructions/native_aot.md#our-typescript-dialect-strict-mode`](./instructions/native_aot.md#our-typescript-dialect-strict-mode).

### 5.5 Async Runtime

Moved to [`instructions/native_aot.md#55-async-runtime`](./instructions/native_aot.md#55-async-runtime).
