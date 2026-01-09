// META: script=/resources/testharness.js

// Curated selector semantics checks for the vm-js WPT runner. These tests rely on the host DOM
// shim and report directly via `__fastrender_wpt_report`.

function report_pass() {
  __fastrender_wpt_report({ file_status: "pass" });
}

function report_fail(message) {
  __fastrender_wpt_report({ file_status: "fail", message: message });
}

var failed = false;

function fail(message) {
  if (failed) return;
  failed = true;
  report_fail(message);
}

function assert_true(cond, message) {
  if (failed) return;
  if (!cond) fail(message);
}

function assert_false(cond, message) {
  if (failed) return;
  if (cond) fail(message);
}

function assert_equals(actual, expected, message) {
  if (failed) return;
  if (actual !== expected) fail(message);
}

function clear_children(node) {
  while (node.childNodes.length !== 0) {
    node.removeChild(node.childNodes[0]);
  }
}

function run() {
  var body = document.body;

  // --- Element.matches: simple selectors ---
  clear_children(body);

  var container = document.createElement("div");
  container.id = "container";
  container.className = "wrap";
  body.appendChild(container);

  var target = document.createElement("span");
  target.id = "target";
  target.className = "inner";
  container.appendChild(target);

  assert_true(target.matches(".inner"), "expected .inner to match the element");
  assert_true(target.matches("span"), "expected tag selector to match");
  assert_true(target.matches("#target"), "expected id selector to match");
  assert_false(target.matches("#nope"), "expected non-matching id selector to return false");

  if (failed) return;

  // --- Element.matches: descendant combinator ---
  clear_children(body);

  container = document.createElement("div");
  container.id = "container";
  body.appendChild(container);

  target = document.createElement("span");
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

  if (failed) return;

  // --- Element.closest ---
  clear_children(body);

  container = document.createElement("div");
  container.id = "container";
  body.appendChild(container);

  target = document.createElement("span");
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
  assert_equals(target.closest("section"), null, "expected closest to return null when no ancestor matches");

  if (failed) return;

  // --- invalid selectors throw SyntaxError ---
  clear_children(body);

  var el = document.createElement("div");
  body.appendChild(el);

  var threw = false;
  var name = "";
  try {
    el.matches("div[");
  } catch (e) {
    threw = true;
    name = e.name;
  }
  assert_true(threw, "expected matches() to throw for invalid selectors");
  assert_equals(name, "SyntaxError", "expected a SyntaxError from matches()");

  threw = false;
  name = "";
  try {
    el.closest("div[");
  } catch (e) {
    threw = true;
    name = e.name;
  }
  assert_true(threw, "expected closest() to throw for invalid selectors");
  assert_equals(name, "SyntaxError", "expected a SyntaxError from closest()");
}

run();

if (!failed) {
  report_pass();
}
