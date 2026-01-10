// META: script=/resources/testharness.js

// Curated selector traversal checks for the vm-js WPT runner. These tests rely on the host DOM
// shim.

function clear_children(node) {
  while (node.childNodes.length !== 0) {
    node.removeChild(node.childNodes[0]);
  }
}

test(() => {
  const body = document.body;
  clear_children(body);

  const a = document.createElement("div");
  a.id = "a";
  body.appendChild(a);

  const a_inner = document.createElement("span");
  a_inner.id = "a_inner";
  a_inner.className = "inner";
  a.appendChild(a_inner);

  const b = document.createElement("div");
  b.id = "b";
  body.appendChild(b);

  const b_inner = document.createElement("span");
  b_inner.id = "b_inner";
  b_inner.className = "inner";
  b.appendChild(b_inner);

  const found_a = body.querySelector("#a");
  assert_true(found_a !== null, "expected to find #a under document.body");

  const inner = found_a.querySelector(".inner");
  assert_true(inner !== null, "expected to find .inner under #a");
  assert_equals(inner.id, "a_inner", "Element.querySelector must be scoped to the element's subtree");
}, "Element.querySelector scopes traversal to the element's subtree");

test(() => {
  const body = document.body;
  clear_children(body);

  const a = document.createElement("div");
  a.id = "a";
  body.appendChild(a);

  try {
    a.querySelector("div[");
    assert_unreached("expected invalid selector to throw");
  } catch (e) {
    assert_equals(e.name, "SyntaxError", "expected a SyntaxError");
  }
}, "Element.querySelector throws SyntaxError on invalid selectors");

test(() => {
  const body = document.body;
  clear_children(body);

  const a = document.createElement("div");
  a.id = "a";
  body.appendChild(a);

  const a_inner = document.createElement("span");
  a_inner.id = "a_inner";
  a_inner.className = "inner";
  a.appendChild(a_inner);

  const matches = a.querySelectorAll(".inner");
  assert_equals(matches.length, 1, "expected one match under #a");
  assert_equals(matches[0].id, "a_inner", "expected the match to be the inner span");
}, "Element.querySelectorAll returns an array-like object of scoped matches");

test(() => {
  const body = document.body;
  clear_children(body);

  const a = document.createElement("div");
  a.id = "a";
  body.appendChild(a);

  const scope_el = a.querySelector(":scope");
  assert_equals(scope_el, a, "expected :scope to match the element itself");

  const matches = a.querySelectorAll(":scope");
  assert_equals(matches.length, 1, "expected one :scope match");
  assert_equals(matches[0], a, "expected :scope match to be the element itself");
}, ":scope matches the scoping element");

test(() => {
  const body = document.body;
  clear_children(body);

  const a = document.createElement("div");
  a.id = "a";
  body.appendChild(a);

  const direct = document.createElement("span");
  direct.id = "direct";
  direct.className = "inner other";
  a.appendChild(direct);

  const wrapper = document.createElement("div");
  wrapper.id = "wrapper";
  wrapper.className = "inner";
  a.appendChild(wrapper);

  const nested = document.createElement("span");
  nested.id = "nested";
  nested.className = "inner other";
  wrapper.appendChild(nested);

  const found_direct = a.querySelector(":scope > span.inner.other");
  assert_true(found_direct !== null, "expected to match the direct child span");
  assert_equals(found_direct.id, "direct", "expected child combinator to only match direct children");

  const found_nested = a.querySelector("div#wrapper span.inner");
  assert_true(found_nested !== null, "expected to match the nested span under #wrapper");
  assert_equals(found_nested.id, "nested", "expected to match the nested span");

  const ordered = a.querySelectorAll("#nested, #direct");
  assert_equals(ordered.length, 2, "expected two matches");
  assert_equals(ordered[0].id, "direct", "matches must be returned in tree order");
  assert_equals(ordered[1].id, "nested", "matches must be returned in tree order");
}, "Element.querySelector(All) supports combinators and selector lists");

test(() => {
  const body = document.body;
  clear_children(body);

  const tmpl = document.createElement("template");
  body.appendChild(tmpl);
  const inert_div = document.createElement("div");
  inert_div.id = "inert";
  tmpl.appendChild(inert_div);

  const inert = document.body.querySelector("#inert");
  assert_equals(inert, null, "selector traversal should skip inert <template> contents");
}, "Selector traversal skips inert <template> contents");
