// META: script=/resources/testharness.js

test(() => {
  const out = [];
  for (const x of [1, 2, 3]) {
    out.push(x);
  }
  assert_equals(out.length, 3);
  assert_equals(out[0], 1);
  assert_equals(out[1], 2);
  assert_equals(out[2], 3);
}, "for..of with const does not re-initialize bindings across iterations");

test(() => {
  const fns = [];
  for (let x of [1, 2, 3]) {
    fns.push(() => x);
  }
  assert_equals(fns.length, 3);
  assert_equals(fns[0](), 1);
  assert_equals(fns[1](), 2);
  assert_equals(fns[2](), 3);
}, "for..of with let creates per-iteration bindings (closure capture)");
