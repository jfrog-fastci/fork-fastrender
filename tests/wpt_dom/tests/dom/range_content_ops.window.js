// META: script=/resources/testharness.js
//
// Focused Range content mutation API tests that do not require iframe support.
//
// These are intentionally small and avoid depending on upstream WPT Range .html
// tests (which exercise iframe/document rewriting behavior that is not yet
// supported by the offline runner).
//
// Spec: https://dom.spec.whatwg.org/#interface-range

function clear_children(node) {
  while (node.childNodes.length !== 0) {
    node.removeChild(node.childNodes[0]);
  }
}

test(() => {
  clear_children(document.body);

  const host = document.createElement("div");
  const t = document.createTextNode("abcd");
  host.appendChild(t);
  document.body.appendChild(host);

  const r = document.createRange();
  r.setStart(t, 1);
  r.setEnd(t, 3);

  r.deleteContents();

  assert_equals(t.data, "ad");
  assert_true(r.collapsed);
  assert_equals(r.startContainer, t);
  assert_equals(r.endContainer, t);
  assert_equals(r.startOffset, 1);
  assert_equals(r.endOffset, 1);
}, "Range.deleteContents() deletes within a single Text node and collapses the range");

test(() => {
  clear_children(document.body);

  const host = document.createElement("div");
  document.body.appendChild(host);

  const startText = document.createTextNode("ab");
  const span = document.createElement("span");
  span.appendChild(document.createTextNode("cd"));
  const endText = document.createTextNode("ef");
  host.appendChild(startText);
  host.appendChild(span);
  host.appendChild(endText);

  const r = document.createRange();
  r.setStart(startText, 1); // "b"
  r.setEnd(endText, 1); // "e"

  const frag = r.extractContents();

  assert_equals(frag.nodeType, Node.DOCUMENT_FRAGMENT_NODE);
  assert_equals(frag.childNodes.length, 3);
  assert_equals(frag.childNodes[0].nodeType, Node.TEXT_NODE);
  assert_equals(frag.childNodes[0].data, "b");
  assert_equals(frag.childNodes[1], span);
  assert_equals(frag.childNodes[2].nodeType, Node.TEXT_NODE);
  assert_equals(frag.childNodes[2].data, "e");

  assert_equals(host.childNodes.length, 2);
  assert_equals(host.childNodes[0].nodeType, Node.TEXT_NODE);
  assert_equals(host.childNodes[0].data, "a");
  assert_equals(host.childNodes[1].nodeType, Node.TEXT_NODE);
  assert_equals(host.childNodes[1].data, "f");

  assert_true(r.collapsed);
  assert_equals(r.startContainer, host);
  assert_equals(r.endContainer, host);
  assert_equals(r.startOffset, 1);
  assert_equals(r.endOffset, 1);
}, "Range.extractContents() moves selected content into a DocumentFragment and collapses the range");

test(() => {
  clear_children(document.body);

  const host = document.createElement("div");
  document.body.appendChild(host);

  const startText = document.createTextNode("ab");
  const span = document.createElement("span");
  span.appendChild(document.createTextNode("cd"));
  const endText = document.createTextNode("ef");
  host.appendChild(startText);
  host.appendChild(span);
  host.appendChild(endText);

  const r = document.createRange();
  r.setStart(startText, 1); // "b"
  r.setEnd(endText, 1); // "e"

  const frag = r.cloneContents();

  assert_equals(frag.nodeType, Node.DOCUMENT_FRAGMENT_NODE);
  assert_equals(frag.childNodes.length, 3);
  assert_equals(frag.childNodes[0].nodeType, Node.TEXT_NODE);
  assert_equals(frag.childNodes[0].data, "b");
  assert_not_equals(frag.childNodes[1], span, "cloneContents should clone contained elements");
  assert_equals(frag.childNodes[1].nodeName, "SPAN");
  assert_equals(frag.childNodes[2].nodeType, Node.TEXT_NODE);
  assert_equals(frag.childNodes[2].data, "e");

  // Original tree + range unchanged.
  assert_equals(startText.data, "ab");
  assert_equals(endText.data, "ef");
  assert_equals(span.parentNode, host);
  assert_equals(r.startContainer, startText);
  assert_equals(r.startOffset, 1);
  assert_equals(r.endContainer, endText);
  assert_equals(r.endOffset, 1);
}, "Range.cloneContents() clones selected content without mutating the DOM or range");

test(() => {
  clear_children(document.body);

  const host = document.createElement("div");
  const text = document.createTextNode("ab");
  host.appendChild(text);
  document.body.appendChild(host);

  const r = document.createRange();
  r.setStart(text, 1);
  r.setEnd(text, 1);

  const inserted = document.createElement("span");
  r.insertNode(inserted);

  assert_equals(host.childNodes.length, 3);
  assert_equals(host.childNodes[0].nodeType, Node.TEXT_NODE);
  assert_equals(host.childNodes[0].data, "a");
  assert_equals(host.childNodes[1], inserted);
  assert_equals(host.childNodes[2].nodeType, Node.TEXT_NODE);
  assert_equals(host.childNodes[2].data, "b");

  assert_equals(r.startContainer, text);
  assert_equals(r.startOffset, 1);
  assert_equals(r.endContainer, host);
  assert_equals(r.endOffset, 2);
}, "Range.insertNode() splits Text nodes and expands a collapsed range to include the inserted node");

test(() => {
  clear_children(document.body);

  const host = document.createElement("div");
  const text = document.createTextNode("ab");
  host.appendChild(text);
  document.body.appendChild(host);

  const r = document.createRange();
  r.setStart(text, 1);
  r.setEnd(text, 1);

  const doc2 = Object.create(document);
  const inserted = doc2.createElement("span");
  assert_equals(inserted.ownerDocument, doc2, "sanity: inserted node is initially owned by the alias document");

  r.insertNode(inserted);

  assert_equals(inserted.ownerDocument, document, "insertNode should adopt the inserted node into the range's document");
}, "Range.insertNode() adopts a node created from an alias Document wrapper");

test(() => {
  clear_children(document.body);

  const host = document.createElement("div");
  const text = document.createTextNode("ab");
  host.appendChild(text);
  document.body.appendChild(host);

  const r = document.createRange();
  r.setStart(text, 1);
  r.setEnd(text, 1);

  const doc2 = Object.create(document);
  const frag = doc2.createDocumentFragment();
  const child = doc2.createElement("i");
  frag.appendChild(child);

  assert_equals(frag.ownerDocument, doc2, "sanity: fragment is initially owned by the alias document");
  assert_equals(child.ownerDocument, doc2, "sanity: fragment child is initially owned by the alias document");

  r.insertNode(frag);

  assert_equals(frag.childNodes.length, 0, "insertNode should empty the inserted DocumentFragment");
  assert_equals(frag.ownerDocument, doc2, "insertNode must not adopt the DocumentFragment itself");
  assert_equals(child.ownerDocument, document, "insertNode should adopt fragment children into the range's document");
}, "Range.insertNode() adopts DocumentFragment children but not the fragment itself");

test(() => {
  clear_children(document.body);

  const host = document.createElement("div");
  document.body.appendChild(host);

  const startText = document.createTextNode("ab");
  const span = document.createElement("span");
  span.appendChild(document.createTextNode("cd"));
  const endText = document.createTextNode("ef");
  host.appendChild(startText);
  host.appendChild(span);
  host.appendChild(endText);

  const r = document.createRange();
  r.setStart(startText, 1);
  r.setEnd(endText, 1);

  const wrapper = document.createElement("u");
  r.surroundContents(wrapper);

  assert_equals(host.childNodes.length, 3);
  assert_equals(host.childNodes[1], wrapper);
  assert_equals(wrapper.textContent, "bcde");
  assert_equals(wrapper.childNodes[1], span);

  // The range selects the wrapper.
  assert_equals(r.startContainer, host);
  assert_equals(r.startOffset, 1);
  assert_equals(r.endContainer, host);
  assert_equals(r.endOffset, 2);
}, "Range.surroundContents() wraps selection in a new parent and selects it");

test(() => {
  clear_children(document.body);

  const host = document.createElement("div");
  const t = document.createTextNode("abcd");
  host.appendChild(t);
  document.body.appendChild(host);

  const r = document.createRange();
  r.setStart(t, 1);
  r.setEnd(t, 3); // "bc"

  const doc2 = Object.create(document);
  const wrapper = doc2.createElement("u");
  assert_equals(wrapper.ownerDocument, doc2, "sanity: wrapper is initially owned by the alias document");

  r.surroundContents(wrapper);

  assert_equals(wrapper.ownerDocument, document, "surroundContents should adopt newParent into the range's document");
  assert_equals(wrapper.textContent, "bc");
}, "Range.surroundContents() adopts newParent created from an alias Document wrapper");
