// META: script=/resources/testharness.js

test(() => {
  const el = document.createElement("div");

  assert_true(typeof el.classList === "object");
  assert_false(el.classList.contains("a"));

  el.classList.add("a");
  assert_true(el.classList.contains("a"));
  assert_equals(el.className, "a");

  // Duplicate adds must not affect the token set.
  el.classList.add("a");
  assert_equals(el.className, "a");

  el.classList.add("b");
  assert_equals(el.className, "a b");

  el.classList.remove("a");
  assert_false(el.classList.contains("a"));
  assert_true(el.classList.contains("b"));
  assert_equals(el.className, "b");
}, "Element.classList reflects className and supports add/remove/contains");

test(() => {
  const el = document.createElement("div");

  assert_true(el.classList.toggle("x"));
  assert_equals(el.className, "x");

  assert_false(el.classList.toggle("x"));
  assert_equals(el.className, "");

  assert_false(el.classList.toggle("y", false));
  assert_equals(el.className, "");

  assert_true(el.classList.toggle("y", true));
  assert_equals(el.className, "y");
}, "DOMTokenList.toggle works with and without force");

test(() => {
  const el = document.createElement("div");

  let threw = false;
  let name = "";
  try {
    el.classList.add("");
  } catch (e) {
    threw = true;
    name = e.name;
  }

  assert_true(threw);
  assert_equals(name, "SyntaxError");
}, "DOMTokenList.add throws SyntaxError for empty tokens");

test(() => {
  const el = document.createElement("div");

  let threw = false;
  let name = "";
  try {
    el.classList.add("a b");
  } catch (e) {
    threw = true;
    name = e.name;
  }

  assert_true(threw);
  assert_equals(name, "InvalidCharacterError");
}, "DOMTokenList.add throws InvalidCharacterError for tokens containing ASCII whitespace");

test(() => {
  const el = document.createElement("div");

  el.className = "a b";
  assert_equals(el.classList[0], "a");
  assert_equals(el.classList[1], "b");
  assert_equals(el.classList[2], undefined);

  el.classList.remove("a");
  assert_equals(el.classList[0], "b");
  assert_equals(el.classList[1], undefined);

  el.classList.add("c");
  assert_equals(el.classList[0], "b");
  assert_equals(el.classList[1], "c");

  el.className = "x y";
  assert_equals(el.classList[0], "x");
  assert_equals(el.classList[1], "y");
}, "DOMTokenList supports indexed property access");
