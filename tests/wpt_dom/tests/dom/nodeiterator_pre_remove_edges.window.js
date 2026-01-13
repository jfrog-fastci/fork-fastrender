// META: script=/resources/testharness.js
//
// Additional NodeIterator "pre-remove steps" coverage for tricky tree-traversal edges:
// - ShadowRoot internal host->shadowRoot edge must be ignored when searching for preceding nodes
// - Inert <template> contents must be ignored when searching for preceding nodes
// - Moving nodes via appendChild() must still run pre-remove updates
//
// Spec: https://dom.spec.whatwg.org/#nodeiterator-pre-removing-steps
// Traversal tree boundaries: https://dom.spec.whatwg.org/#traversal

function clear_children(node) {
  while (node.childNodes.length !== 0) {
    node.removeChild(node.childNodes[0]);
  }
}

test(() => {
  clear_children(document.body);

  const container = document.createElement("div");
  const host = document.createElement("div");
  const shadow = host.attachShadow({ mode: "open" });
  shadow.appendChild(document.createElement("span"));

  const after = document.createElement("p");
  container.appendChild(host);
  container.appendChild(after);
  document.body.appendChild(container);

  const it = document.createNodeIterator(container, NodeFilter.SHOW_ELEMENT, null);

  it.nextNode(); // container
  it.nextNode(); // host
  it.nextNode(); // after
  it.previousNode(); // after (toggle pointerBeforeReferenceNode => true)

  assert_equals(it.referenceNode, after);
  assert_true(it.pointerBeforeReferenceNode);

  // Removing the last node forces the iterator to fall back to the preceding node. That preceding
  // node must be the host element itself, not a descendant inside its shadow root.
  container.removeChild(after);

  assert_equals(it.referenceNode, host);
  assert_false(it.pointerBeforeReferenceNode);
}, "NodeIterator pre-remove: preceding-node search skips descendants inside a shadow root");

test(() => {
  clear_children(document.body);

  const container = document.createElement("div");
  const template = document.createElement("template");
  template.innerHTML = "<span id=inert></span>";
  const after = document.createElement("p");
  container.appendChild(template);
  container.appendChild(after);
  document.body.appendChild(container);

  const it = document.createNodeIterator(container, NodeFilter.SHOW_ELEMENT, null);

  it.nextNode(); // container
  it.nextNode(); // template
  it.nextNode(); // after
  it.previousNode(); // after (toggle pointerBeforeReferenceNode => true)

  assert_equals(it.referenceNode, after);
  assert_true(it.pointerBeforeReferenceNode);

  // Removing the last node forces the iterator to fall back to the preceding node. The preceding
  // node must be the <template> element itself, not any inert descendants.
  container.removeChild(after);

  assert_equals(it.referenceNode, template);
  assert_false(it.pointerBeforeReferenceNode);
}, "NodeIterator pre-remove: preceding-node search treats <template> as a leaf (skips inert contents)");

test(() => {
  clear_children(document.body);

  const host = document.createElement("div");
  const shadow = host.attachShadow({ mode: "open" });
  shadow.appendChild(document.createElement("span"));

  const a = document.createElement("a");
  const b = document.createElement("b");
  a.appendChild(b);
  host.appendChild(a);
  document.body.appendChild(host);

  const it = document.createNodeIterator(host, NodeFilter.SHOW_ELEMENT, null);

  it.nextNode(); // host
  it.nextNode(); // a
  it.nextNode(); // b
  it.previousNode(); // b (toggle pointerBeforeReferenceNode => true)

  assert_equals(it.referenceNode, b);
  assert_true(it.pointerBeforeReferenceNode);

  // Removing `a` should update the iterator to reference the host element, not the internal
  // ShadowRoot (which is stored as a child in dom2 but is not part of the DOM tree traversal).
  host.removeChild(a);

  assert_equals(it.referenceNode, host);
  assert_false(it.pointerBeforeReferenceNode);
}, "NodeIterator pre-remove: previous-sibling search skips the internal ShadowRoot child");

test(() => {
  clear_children(document.body);

  const root = document.createElement("div");
  const parent1 = document.createElement("div");
  const parent2 = document.createElement("div");
  root.appendChild(parent1);
  root.appendChild(parent2);
  document.body.appendChild(root);

  const a = document.createElement("a");
  const b = document.createElement("b");
  parent1.appendChild(a);
  parent1.appendChild(b);

  const it = document.createNodeIterator(parent1, NodeFilter.SHOW_ELEMENT, null);

  it.nextNode(); // parent1
  it.nextNode(); // a
  it.previousNode(); // a (toggle pointerBeforeReferenceNode => true)

  assert_equals(it.referenceNode, a);
  assert_true(it.pointerBeforeReferenceNode);

  // Moving a node is a remove-then-insert. The pre-remove steps should run for the removal from
  // parent1 and update the iterator to the first following node in the old tree (b).
  parent2.appendChild(a);

  assert_equals(it.referenceNode, b);
  assert_true(it.pointerBeforeReferenceNode);
}, "NodeIterator pre-remove: moving a node via appendChild runs pre-remove updates");

