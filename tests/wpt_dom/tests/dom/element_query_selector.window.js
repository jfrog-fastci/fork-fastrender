// META: script=/resources/testharness.js

function clear_children(node) {
  // `childNodes` is a live NodeList in browsers (read-only), but indexable + has a `length`.
  // Our minimal DOM shim represents it as an array, so this works in both worlds.
  while (node.childNodes && node.childNodes.length) {
    node.removeChild(node.childNodes[0]);
  }
}

test(() => {
  const body = document.body;
  clear_children(body);

  const a = document.createElement("div");
  a.id = "a";
  body.appendChild(a);

  const aInner = document.createElement("span");
  aInner.id = "a_inner";
  aInner.className = "inner";
  a.appendChild(aInner);

  const b = document.createElement("div");
  b.id = "b";
  body.appendChild(b);

  const bInner = document.createElement("span");
  bInner.id = "b_inner";
  bInner.className = "inner";
  b.appendChild(bInner);

  const foundA = body.querySelector("#a");
  assert_true(foundA !== null, "expected to find #a under document.body");

  const inner = foundA.querySelector(".inner");
  assert_true(inner !== null, "expected to find .inner under #a");
  assert_equals(
    inner.id,
    "a_inner",
    "Element.querySelector must be scoped to the element's subtree"
  );
}, "Element.querySelector scopes selector matching to the element's subtree");

test(() => {
  const body = document.body;
  clear_children(body);
  const a = document.createElement("div");
  a.id = "a";
  body.appendChild(a);

  let threw = false;
  try {
    a.querySelector("div[");
  } catch (e) {
    threw = true;
    assert_equals(e && e.name, "SyntaxError", "expected a SyntaxError");
  }
  assert_true(threw, "expected invalid selector to throw");
}, "Element.querySelector throws SyntaxError on invalid selectors");

test(() => {
  const body = document.body;
  clear_children(body);

  const a = document.createElement("div");
  a.id = "a";
  body.appendChild(a);

  const aInner = document.createElement("span");
  aInner.id = "a_inner";
  aInner.className = "inner";
  a.appendChild(aInner);

  const matches = a.querySelectorAll(".inner");
  assert_equals(matches.length, 1, "expected one match under #a");
  assert_equals(matches[0].id, "a_inner");
}, "Element.querySelectorAll returns an array of scoped matches");

test(() => {
  const body = document.body;
  clear_children(body);
  const a = document.createElement("div");
  a.id = "a";
  body.appendChild(a);

  const scope = a.querySelector(":scope");
  assert_equals(scope, a, "expected :scope to match the element itself");

  const matches = a.querySelectorAll(":scope");
  assert_equals(matches.length, 1, "expected one :scope match");
  assert_equals(matches[0], a, "expected :scope match to be the element itself");
}, "Element.querySelector(All) supports :scope");

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

  const foundDirect = a.querySelector(":scope > span.inner.other");
  assert_true(foundDirect !== null, "expected to match the direct child span");
  assert_equals(foundDirect.id, "direct", "expected child combinator to only match direct children");

  const foundNested = a.querySelector("div#wrapper span.inner");
  assert_true(foundNested !== null, "expected to match the nested span under #wrapper");
  assert_equals(foundNested.id, "nested");

  const ordered = a.querySelectorAll("#nested, #direct");
  assert_equals(ordered.length, 2, "expected two matches");
  assert_equals(ordered[0].id, "direct", "matches must be returned in tree order");
  assert_equals(ordered[1].id, "nested", "matches must be returned in tree order");
}, "Element.querySelector(All) supports child combinators, compound selectors, and selector lists");

test(() => {
  const body = document.body;
  clear_children(body);

  const tmpl = document.createElement("template");
  body.appendChild(tmpl);
  const inertDiv = document.createElement("div");
  inertDiv.id = "inert";
  tmpl.appendChild(inertDiv);

  const inert = document.body.querySelector("#inert");
  assert_equals(
    inert,
    null,
    "selector traversal should skip inert <template> contents"
  );
}, "Element.querySelector skips inert template subtrees");
