// META: script=/resources/testharness.js

test(() => {
  const frag = document.createDocumentFragment();
  assert_true(frag instanceof DocumentFragment);
}, "document.createDocumentFragment() returns a DocumentFragment");

test(() => {
  const frag = document.createDocumentFragment();
  if (typeof frag.appendChild !== "function") return;

  const a = document.createElement("span");
  a.innerHTML = "one";
  const b = document.createElement("span");
  b.innerHTML = "two";

  frag.appendChild(a);
  frag.appendChild(b);

  const host = document.createElement("div");
  host.appendChild(frag);

  assert_equals(host.innerHTML, "<span>one</span><span>two</span>");

  assert_equals(frag.childNodes.length, 0, "fragment should be emptied after insertion");
}, "DocumentFragment appendChild + append into host moves children");
