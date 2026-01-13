# IndexedDB MVP (presence shim)

FastRender **does not implement IndexedDB storage** yet.

However, many real-world JS bundles treat a missing `indexedDB` global as a hard failure and only
fall back to other storage mechanisms (e.g. `localStorage`) if `indexedDB.open(..)` exists but fails.
To avoid breaking those pages, FastRender installs a small **presence shim** into every `vm-js`
Window realm (`src/js/vmjs/window_indexed_db.rs`).

This document describes **what exists today** (and what does not) so future work on “real”
IndexedDB doesn’t accidentally assume features are already supported.

## Supported globals / methods

The shim only installs when `globalThis.indexedDB` is **missing**.

### `indexedDB` / `IDBFactory`

`globalThis.indexedDB` is an object with the following:

- `indexedDB.open(name, version?) -> IDBOpenDBRequest`
- `indexedDB.deleteDatabase(name) -> IDBOpenDBRequest`
- `indexedDB.cmp(a, b)` exists but **throws** `NotSupportedError` synchronously.

Vendor aliases are also defined and all point at the same object:

- `webkitIndexedDB`
- `mozIndexedDB`
- `msIndexedDB`
- `OIndexedDB`

### Constructor globals (surface-only)

The shim defines these globals so `typeof X === "function"` and `instanceof` checks behave
plausibly:

- `IDBFactory`
- `IDBRequest`
- `IDBOpenDBRequest` (prototype-chain extends `IDBRequest`)
- `IDBDatabase`
- `IDBTransaction`
- `IDBObjectStore`
- `IDBKeyRange`
- `IDBVersionChangeEvent`

All of these are **illegal constructors**: calling them throws `TypeError("Illegal constructor")`.

### `IDBKeyRange`

Static helpers exist but all **throw** `NotSupportedError`:

- `IDBKeyRange.only(..)`
- `IDBKeyRange.lowerBound(..)`
- `IDBKeyRange.upperBound(..)`
- `IDBKeyRange.bound(..)`

## Key types and ordering

Not supported.

Because there is no database, FastRender does not currently accept/compare/normalize keys:

- `indexedDB.cmp(a, b)` throws `NotSupportedError`.
- No `IDBKeyRange` objects can be constructed (the constructor is illegal, and all factory methods
  throw).

## Stored value types (structured clone subset)

Not supported.

The shim never persists any values, so it never performs structured cloning. (`structuredClone()`
exists independently; see `src/js/vmjs/window_structured_clone.rs`.)

## Transaction behavior

Not supported (no storage, no `IDBDatabase` objects, no `transaction()` API).

What *is* implemented is the **async delivery shape** of request failure:

- `indexedDB.open(..)` and `indexedDB.deleteDatabase(..)` **do not throw synchronously**.
- They return a request-shaped object immediately with:
  - `readyState: "pending"`
  - `result: undefined`
  - `error: null`
- In a later **microtask**, the request transitions to:
  - `readyState: "done"`
  - `result: undefined`
  - `error: DOMException(name="NotSupportedError")` (or a `{ name, message }` fallback if
    `DOMException` is unavailable)
  - An `error` event is dispatched.

Microtask scheduling is intentionally deterministic:

- Prefer the internal FastRender hook `globalThis.__fastrender_queue_microtask` (wired to the host
  event loop).
- Else use `queueMicrotask`.
- Else fall back to `Promise.resolve().then(..)`.

### Event/listener semantics

Requests are *not* real DOM `EventTarget`s; they only provide:

- `req.onerror = fn` attribute handler (and other standard IDB request attributes like `onsuccess`,
  `onupgradeneeded`, `onblocked`, though only `onerror` is ever invoked).
- `req.addEventListener(type, fn)`
- `req.removeEventListener(type, fn)`

On dispatch, listener ordering is deterministic:

1. The attribute handler `req["on" + type]` runs first.
2. Then listeners registered via `addEventListener`, in insertion order.

Exceptions thrown by handlers/listeners are **swallowed** so later listeners still run.

The event object is a small plain object:

- `{ type, target, currentTarget }`

## Known gaps / limitations

This is intentionally a **non-functional** IndexedDB implementation:

- No object stores, indexes, cursors, key generators, version upgrades, or transactions.
- No `success`/`upgradeneeded`/`blocked` events (requests always go to `error`).
- No `versionchange` events, close semantics, `IDBDatabase` lifetime tracking, or concurrency model.
- No persistence across page loads, tabs, threads, or processes.

## Testing / reset helpers

- Shim behavior is covered by `tests/misc/js_indexed_db_shim.rs`.
- The shim is **stateless**, so there is no `clear_default_indexeddb_hub_for_tests` today.

If/when IndexedDB storage is implemented with a thread-local backend (similar to
`src/js/web_storage.rs`), we will likely need a **test-only reset helper** because Rust’s test
harness may reuse worker threads between tests, which can otherwise leak thread-local state.
