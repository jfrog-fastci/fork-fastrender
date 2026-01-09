// META: script=/resources/testharness.js

// Curated selector traversal checks for the vm-js WPT runner. These tests rely on the host DOM
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

  // --- querySelector scopes to the element's subtree ---
  clear_children(body);

  var a = document.createElement("div");
  a.id = "a";
  body.appendChild(a);

  var a_inner = document.createElement("span");
  a_inner.id = "a_inner";
  a_inner.className = "inner";
  a.appendChild(a_inner);

  var b = document.createElement("div");
  b.id = "b";
  body.appendChild(b);

  var b_inner = document.createElement("span");
  b_inner.id = "b_inner";
  b_inner.className = "inner";
  b.appendChild(b_inner);

  var found_a = body.querySelector("#a");
  assert_true(found_a !== null, "expected to find #a under document.body");

  var inner = found_a.querySelector(".inner");
  assert_true(inner !== null, "expected to find .inner under #a");
  assert_equals(inner.id, "a_inner", "Element.querySelector must be scoped to the element's subtree");

  if (failed) return;

  // --- querySelector throws SyntaxError on invalid selectors ---
  clear_children(body);

  a = document.createElement("div");
  a.id = "a";
  body.appendChild(a);

  var threw = false;
  var name = "";
  try {
    a.querySelector("div[");
  } catch (e) {
    threw = true;
    name = e.name;
  }
  assert_true(threw, "expected invalid selector to throw");
  assert_equals(name, "SyntaxError", "expected a SyntaxError");

  if (failed) return;

  // --- querySelectorAll returns an array-like object of scoped matches ---
  clear_children(body);

  a = document.createElement("div");
  a.id = "a";
  body.appendChild(a);

  a_inner = document.createElement("span");
  a_inner.id = "a_inner";
  a_inner.className = "inner";
  a.appendChild(a_inner);

  var matches = a.querySelectorAll(".inner");
  assert_equals(matches.length, 1, "expected one match under #a");
  assert_equals(matches[0].id, "a_inner", "expected the match to be the inner span");

  if (failed) return;

  // --- :scope ---
  clear_children(body);

  a = document.createElement("div");
  a.id = "a";
  body.appendChild(a);

  var scope_el = a.querySelector(":scope");
  assert_equals(scope_el, a, "expected :scope to match the element itself");

  matches = a.querySelectorAll(":scope");
  assert_equals(matches.length, 1, "expected one :scope match");
  assert_equals(matches[0], a, "expected :scope match to be the element itself");

  if (failed) return;

  // --- child combinators, compound selectors, and selector lists ---
  clear_children(body);

  a = document.createElement("div");
  a.id = "a";
  body.appendChild(a);

  var direct = document.createElement("span");
  direct.id = "direct";
  direct.className = "inner other";
  a.appendChild(direct);

  var wrapper = document.createElement("div");
  wrapper.id = "wrapper";
  wrapper.className = "inner";
  a.appendChild(wrapper);

  var nested = document.createElement("span");
  nested.id = "nested";
  nested.className = "inner other";
  wrapper.appendChild(nested);

  var found_direct = a.querySelector(":scope > span.inner.other");
  assert_true(found_direct !== null, "expected to match the direct child span");
  assert_equals(found_direct.id, "direct", "expected child combinator to only match direct children");

  var found_nested = a.querySelector("div#wrapper span.inner");
  assert_true(found_nested !== null, "expected to match the nested span under #wrapper");
  assert_equals(found_nested.id, "nested", "expected to match the nested span");

  var ordered = a.querySelectorAll("#nested, #direct");
  assert_equals(ordered.length, 2, "expected two matches");
  assert_equals(ordered[0].id, "direct", "matches must be returned in tree order");
  assert_equals(ordered[1].id, "nested", "matches must be returned in tree order");

  if (failed) return;

  // --- template contents are inert ---
  clear_children(body);

  var tmpl = document.createElement("template");
  body.appendChild(tmpl);
  var inert_div = document.createElement("div");
  inert_div.id = "inert";
  tmpl.appendChild(inert_div);

  var inert = document.body.querySelector("#inert");
  assert_equals(inert, null, "selector traversal should skip inert <template> contents");
}

run();

if (!failed) {
  report_pass();
}
