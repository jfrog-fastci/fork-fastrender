// META: script=/resources/testharness.js

test(() => {
  const params = new URLSearchParams([["a", "1"], ["b", "2"]]);
  assert_equals(params.toString(), "a=1&b=2");
}, "URLSearchParams constructor accepts sequence<sequence<USVString>> init");

test(() => {
  const params = new URLSearchParams({ a: "1", b: "2" });
  assert_equals(params.toString(), "a=1&b=2");
}, "URLSearchParams constructor accepts record<USVString, USVString> init");

test(() => {
  const first = new URLSearchParams("a=1&b=2");
  const second = new URLSearchParams(first);
  assert_equals(second.toString(), "a=1&b=2");
}, "URLSearchParams constructor accepts iterable URLSearchParams init");

test(() => {
  const assert_invalid = init => {
    let threw = false;
    try {
      new URLSearchParams(init);
    } catch (e) {
      threw = true;
      assert_equals(e.name, "TypeError");
    }
    assert_true(threw, "expected invalid init to throw");
  };

  assert_invalid([["a"]]);
  assert_invalid([["a", "b", "c"]]);
}, "URLSearchParams sequence init validates that each entry has length 2");

test(() => {
  const params = new URLSearchParams("a=1&b=2");
  const iter = params[Symbol.iterator]();
  const first = iter.next();
  assert_false(first.done);
  assert_true(Array.isArray(first.value));
  assert_equals(first.value[0], "a");
  assert_equals(first.value[1], "1");
}, "URLSearchParams is iterable via @@iterator");
