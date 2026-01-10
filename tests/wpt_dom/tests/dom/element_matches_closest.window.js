// META: script=/resources/testharness.js

// Curated selector semantics checks for the vm-js WPT runner. These tests rely on the host DOM
// shim.

function clear_children(node) {
  while (node.childNodes.length !== 0) {
    node.removeChild(node.childNodes[0]);
  }
}

test(() => {
  const body = document.body;
  clear_children(body);

  const container = document.createElement("div");
  container.id = "container";
  container.className = "wrap";
  body.appendChild(container);

  const target = document.createElement("span");
  target.id = "target";
  target.className = "inner";
  container.appendChild(target);

  assert_true(target.matches(".inner"), "expected .inner to match the element");
  assert_true(target.matches("span"), "expected tag selector to match");
  assert_true(target.matches("#target"), "expected id selector to match");
  assert_false(target.matches("#nope"), "expected non-matching id selector to return false");
}, "Element.matches supports simple selectors");

test(() => {
  const body = document.body;
  clear_children(body);

  const container = document.createElement("div");
  container.id = "container";
  body.appendChild(container);

  const target = document.createElement("span");
  target.id = "target";
  container.appendChild(target);

  assert_true(
    target.matches("div span"),
    "expected descendant combinator selector to match based on ancestors"
  );
  assert_false(
    target.matches("section span"),
    "expected selector requiring a missing ancestor to not match"
  );
}, "Element.matches supports descendant combinators");

test(() => {
  const body = document.body;
  clear_children(body);

  const container = document.createElement("div");
  container.id = "container";
  body.appendChild(container);

  const target = document.createElement("span");
  target.id = "target";
  target.className = "inner";
  container.appendChild(target);

  assert_equals(
    target.closest("#container"),
    container,
    "expected closest to return the nearest matching ancestor"
  );
  assert_equals(target.closest(".inner"), target, "closest should be inclusive of the element itself");
  assert_equals(target.closest("body"), body, "expected closest to find <body> ancestor");
  assert_equals(
    target.closest("section"),
    null,
    "expected closest to return null when no ancestor matches"
  );
}, "Element.closest returns the nearest matching ancestor (inclusive)");

test(() => {
  const body = document.body;
  clear_children(body);

  const el = document.createElement("div");
  body.appendChild(el);

  let threw = false;
  let name = "";
  try {
    el.matches("div[");
  } catch (e) {
    threw = true;
    name = e.name;
  }
  assert_true(threw, "expected matches() to throw for invalid selectors");
  assert_equals(name, "SyntaxError", "expected a SyntaxError from matches()");
}, "Element.matches throws SyntaxError on invalid selectors");

test(() => {
  const body = document.body;
  clear_children(body);

  const el = document.createElement("div");
  body.appendChild(el);

  let threw = false;
  let name = "";
  try {
    el.closest("div[");
  } catch (e) {
    threw = true;
    name = e.name;
  }
  assert_true(threw, "expected closest() to throw for invalid selectors");
  assert_equals(name, "SyntaxError", "expected a SyntaxError from closest()");
}, "Element.closest throws SyntaxError on invalid selectors");
