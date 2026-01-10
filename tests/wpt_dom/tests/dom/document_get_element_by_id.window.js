// META: script=/resources/testharness.js
//
// Curated `Document.getElementById` semantics checks.

function clear_children(node) {
  while (node.childNodes.length !== 0) {
    node.removeChild(node.childNodes[0]);
  }
}


test(() => {
  clear_children(document.body);
  assert_equals(
    document.getElementById("missing"),
    null,
    "getElementById should return null when the element does not exist"
  );
}, "Document.getElementById returns null when missing");

test(() => {
  clear_children(document.body);

  const a = document.createElement("div");
  a.id = "a";
  document.body.appendChild(a);

  const b = document.createElement("div");
  b.id = "b";
  document.body.appendChild(b);

  assert_equals(document.getElementById("a"), a, "expected getElementById('a') to return the element");
  assert_equals(document.getElementById("b"), b, "expected getElementById('b') to return the element");
}, "Document.getElementById finds elements by id");

test(() => {
  clear_children(document.body);

  const a = document.createElement("div");
  a.id = "a";
  document.body.appendChild(a);

  // Duplicate ids are invalid HTML but allowed in practice; `getElementById` returns the first
  // matching element in tree order.
  const c = document.createElement("div");
  c.id = "a";
  document.body.appendChild(c);

  assert_equals(
    document.getElementById("a"),
    a,
    "expected getElementById to return the first element in tree order when ids collide"
  );
}, "Document.getElementById returns the first match when ids collide");

test(() => {
  clear_children(document.body);

  // `<template>` contents are inert and must not be searched.
  const tmpl = document.createElement("template");
  tmpl.id = "tmpl";
  document.body.appendChild(tmpl);

  const inside = document.createElement("div");
  inside.id = "inside";
  tmpl.appendChild(inside);

  assert_equals(document.getElementById("tmpl"), tmpl, "template element itself should be findable");
  assert_equals(
    document.getElementById("inside"),
    null,
    "getElementById must not search inside inert <template> contents"
  );
}, "Document.getElementById does not search inside <template> contents");

test(() => {
  clear_children(document.body);

  // Detached DocumentFragments should be searchable via `fragment.getElementById`, but their
  // contents should not be visible to `document.getElementById` until inserted.
  const frag = document.createDocumentFragment();
  const f1 = document.createElement("div");
  f1.id = "f1";
  frag.appendChild(f1);

  const f2 = document.createElement("div");
  f2.id = "f2";
  frag.appendChild(f2);

  assert_equals(
    frag.getElementById("missing"),
    null,
    "fragment.getElementById should return null for missing ids"
  );
  assert_equals(frag.getElementById("f1"), f1, "fragment.getElementById should find descendants");
  assert_equals(frag.getElementById("f2"), f2, "fragment.getElementById should find descendants");
  assert_equals(
    document.getElementById("f1"),
    null,
    "document.getElementById must not search detached fragment contents"
  );
}, "DocumentFragment.getElementById searches fragment contents, but document does not");
