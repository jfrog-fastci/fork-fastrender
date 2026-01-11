# Async runtime ABI (runtime-native)

This crate provides a small host-side async runtime intended for native embeddings.

## Microtask draining

Two entry points execute pending work:

- `rt_drain_microtasks() -> bool`
- `rt_async_run_until_idle() -> bool`

Both functions are **non-reentrant** by design (HTML-style microtask checkpoint semantics).
If either function is called while a drain is already in progress (directly or indirectly, e.g.
from within a microtask), the nested call is treated as a **no-op** and returns `false`.

