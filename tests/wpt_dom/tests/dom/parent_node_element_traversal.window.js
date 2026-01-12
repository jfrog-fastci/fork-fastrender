// META: script=/resources/testharness.js
//
// `ParentNode` and `NonDocumentTypeChildNode` element-only traversal properties.
// - ParentNode.children / childElementCount / firstElementChild / lastElementChild
// - Element.nextElementSibling / previousElementSibling

function clear_children(node) {
  while (node.childNodes.length !== 0) {
    node.removeChild(node.childNodes[0]);
  }
}

test(() => {
  clear_children(document.body);

  const parent = document.createElement("div");
  document.body.appendChild(parent);

  const t = document.createTextNode("x");
  const a = document.createElement("span");
  a.id = "a";
  const c = document.createComment("c");
  const b = document.createElement("span");
  b.id = "b";

  parent.appendChild(t);
  parent.appendChild(a);
  parent.appendChild(c);
  parent.appendChild(b);

  const kids = parent.children;
  assert_equals(kids, parent.children, "children should be [SameObject]");
  assert_true(kids instanceof HTMLCollection, "children should be an HTMLCollection");

  assert_equals(kids.length, 2, "children should skip non-element nodes");
  assert_equals(kids[0], a, "children[0] should be the first element child");
  assert_equals(kids[1], b, "children[1] should be the second element child");
  assert_equals(kids.item(0), a, "children.item(0) should match children[0]");
  assert_equals(kids.item(99), null, "children.item() returns null when out of range");

  assert_equals(parent.childElementCount, 2, "childElementCount should count element children");
  assert_equals(parent.firstElementChild, a, "firstElementChild should skip non-elements");
  assert_equals(parent.lastElementChild, b, "lastElementChild should skip non-elements");
}, "ParentNode children skips non-elements and is SameObject");

test(() => {
  clear_children(document.body);

  const parent = document.createElement("div");
  document.body.appendChild(parent);

  const a = document.createElement("span");
  a.id = "a";
  parent.appendChild(a);

  const kids = parent.children;
  assert_equals(kids.length, 1);

  const b = document.createElement("span");
  b.id = "b";
  parent.appendChild(b);
  assert_equals(kids.length, 2, "children collection should be live after appendChild");
  assert_equals(kids[0], a);
  assert_equals(kids[1], b);

  parent.removeChild(a);
  assert_equals(kids.length, 1, "children collection should be live after removeChild");
  assert_equals(kids[0], b);
}, "HTMLCollection returned from children is live across append/remove");

test(() => {
  clear_children(document.body);

  const parent = document.createElement("div");
  document.body.appendChild(parent);

  const a = document.createElement("span");
  a.id = "a";
  const text = document.createTextNode("x");
  const b = document.createElement("span");
  b.id = "b";
  const comment = document.createComment("y");
  const c = document.createElement("span");
  c.id = "c";

  parent.appendChild(a);
  parent.appendChild(text);
  parent.appendChild(b);
  parent.appendChild(comment);
  parent.appendChild(c);

  assert_equals(a.previousElementSibling, null, "previousElementSibling skips non-elements");
  assert_equals(a.nextElementSibling, b, "nextElementSibling skips non-elements");
  assert_equals(b.previousElementSibling, a);
  assert_equals(b.nextElementSibling, c);
  assert_equals(c.previousElementSibling, b);
  assert_equals(c.nextElementSibling, null);
}, "Element nextElementSibling/previousElementSibling skip non-elements");

test(() => {
  clear_children(document.body);

  const tmpl = document.createElement("template");
  document.body.appendChild(tmpl);

  const inside = document.createElement("div");
  inside.id = "inside";
  tmpl.appendChild(inside);

  assert_equals(tmpl.children.length, 0, "template element must not expose inert contents via .children");
  assert_equals(tmpl.firstElementChild, null, "template element must not expose inert contents");
  assert_equals(tmpl.lastElementChild, null, "template element must not expose inert contents");
  assert_equals(tmpl.childElementCount, 0, "template element must not expose inert contents");
}, "ParentNode traversal skips inert <template> contents");

test(() => {
  const kids = document.children;
  assert_true(kids instanceof HTMLCollection);
  assert_equals(kids.length, 1, "Document should have one element child (<html>)");
  assert_equals(kids[0], document.documentElement);
  assert_equals(document.firstElementChild, document.documentElement);
  assert_equals(document.lastElementChild, document.documentElement);
  assert_equals(document.childElementCount, 1);
}, "ParentNode traversal works on Document");

test(() => {
  const frag = document.createDocumentFragment();
  const t = document.createTextNode("x");
  const a = document.createElement("div");
  a.id = "a";
  frag.appendChild(t);
  frag.appendChild(a);

  const kids = frag.children;
  assert_true(kids instanceof HTMLCollection);
  assert_equals(kids.length, 1);
  assert_equals(kids[0], a);
  assert_equals(frag.firstElementChild, a);
  assert_equals(frag.lastElementChild, a);
  assert_equals(frag.childElementCount, 1);
}, "ParentNode traversal works on DocumentFragment");

