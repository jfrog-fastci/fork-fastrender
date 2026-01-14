# test262 unary operator bucket — investigation & minimal repros

`progress/test262/latest_summary.md` historically listed **`unimplemented: unary operator` (~58)**
as a top mismatch reason. Re-running the curated suite on current `main` no longer produces that
reason (0 hits). The failures that look like the same underlying “`yield` reached an unsupported
execution path” issue are now split into more specific buckets:

- `unimplemented: yield in expression type` (previously 11) — fixed in this change
- `unimplemented: yield in for-of binding pattern` (48) — fixed

This doc records the **AST shapes** + **minimal JS repro snippets** for those buckets.

---

## Bucket: `unimplemented: yield in expression type` (fixed)

### Representative failing tests (before fix)

- `language/statements/class/definition/methods-gen-yield-as-expression-with-rhs.js` (strict+non-strict)
- `language/statements/class/definition/methods-gen-yield-as-expression-without-rhs.js` (strict+non-strict)
- `language/statements/generators/yield-spread-arr-single.js` (strict+non-strict)
- `language/statements/generators/yield-spread-arr-multiple.js` (strict+non-strict)
- `language/statements/generators/yield-spread-obj.js` (strict+non-strict)
- `language/statements/generators/yield-identifier-spread-non-strict.js` (non-strict)

### Minimal repro snippets + expected behavior

#### 1) `yield` inside an **array literal** (including spreads)

```js
function* g() {
  // should yield 1, then finish (expression statement result ignored)
  [yield 1];
}
```

Expected:
- `g().next()` yields `{ value: 1, done: false }`
- `g().next()` then yields `{ value: undefined, done: true }`

#### 2) `yield` inside an **object literal spread**

```js
function* g() {
  // should yield 1, resume with an object, and then spread it
  ({ ...yield 1 });
}
```

Expected:
- first `.next()` yields `1`
- second `.next({a: 1})` resumes and completes normally

#### 3) `yield` inside a **comma (sequence) expression**

```js
function* g() {
  // should yield 1, then yield 2, then finish
  yield 1, yield 2;
}
```

Expected:
- `.next()` yields `1`
- `.next()` yields `2`
- `.next()` finishes

### Root cause (pre-fix)

The generator evaluator (`gen_eval_expr`) supported `yield` as a standalone unary expression, but
returned `VmError::Unimplemented(...)` when `yield` appeared inside:

- `Expr::LitArr` / `Expr::LitObj`
- `BinaryExpr` with `OperatorName::Comma`

### Fix summary (this PR)

Implement generator-eval support for:

- array literals (`Expr::LitArr`) including holes + spreads
- object literals (`Expr::LitObj`) including computed keys + spreads
- comma operator (`OperatorName::Comma`)

Verification: each of the tests listed above was re-run via `test262-semantic --filter` and now
passes.

---

## Bucket: `unimplemented: yield in for-of binding pattern` (fixed)

### Representative failing tests

All under `language/statements/for-of/dstr/*.js` (strict+non-strict), e.g.:

- `language/statements/for-of/dstr/array-elem-init-yield-expr.js`
- `language/statements/for-of/dstr/obj-id-init-yield-expr.js`
- `language/statements/for-of/dstr/array-rest-yield-expr.js`
- `language/statements/for-of/dstr/*-iter-rtrn-close*.js` (iterator-close semantics)

### Minimal repro (shape)

```js
function* g() {
  for ([x = yield] of [[]]) {
    // empty
  }
}
```

Expected:
- The generator can **suspend** while evaluating the binding pattern (default initializer).
- If the generator is closed while suspended, the RHS iterator must be closed (tests assert
  `IteratorClose` behavior via `.return()` calls).

### Fix summary

Generator `for..of` now supports suspending within the LHS binding pattern (including defaults,
computed keys, rest patterns) and preserves iterator-close semantics when the generator is closed
while suspended.

Verification:
- `vendor/ecma-rs/vm-js/tests/generators_for_of_yield_in_lhs.rs`
