// META: script=/resources/testharness.js
 
test(() => {
  const frag = document.createDocumentFragment();
  const span = document.createElement("span");
  frag.appendChild(span);
 
  assert_equals(frag.childNodes.length, 1, "fragment should contain the <span> before replacement");
  assert_equals(span.parentNode, frag, "<span> should be parented by the fragment");
 
  span.outerHTML = "<div></div>";
 
  assert_equals(
    frag.childNodes.length,
    1,
    "outerHTML replacement should keep fragment child count stable"
  );
  assert_equals(frag.firstChild.tagName, "DIV", "fragment child should be replaced in-tree");
  assert_equals(frag.firstChild.outerHTML, "<div></div>", "outerHTML getter should serialize the replacement");
  assert_equals(span.parentNode, null, "replaced node should be detached");
}, "Element.outerHTML setter replaces node when parent is a DocumentFragment");
 
test(() => {
  const el = document.createElement("p");
  const before = el.outerHTML;
 
  el.outerHTML = "<span></span>";
 
  assert_equals(el.outerHTML, before, "detached element outerHTML setter should be a no-op");
  assert_equals(el.parentNode, null, "detached element should remain detached");
}, "Element.outerHTML setter is a no-op for detached elements");
 
test(() => {
  const parent = document.createElement("div");
  const frag = document.createDocumentFragment();
  const a = document.createElement("span");
  const b = document.createElement("b");
 
  frag.appendChild(a);
  frag.appendChild(b);
 
  const ret = parent.appendChild(frag);
  assert_equals(ret, frag, "appendChild should return the argument (DocumentFragment)");
 
  assert_equals(frag.parentNode, null, "DocumentFragment itself is not inserted");
  assert_equals(frag.childNodes.length, 0, "DocumentFragment should be emptied on insertion");
 
  assert_equals(parent.childNodes.length, 2, "fragment children should be inserted into the parent");
  assert_equals(parent.childNodes[0].tagName, "SPAN", "first inserted child should be preserved");
  assert_equals(parent.childNodes[1].tagName, "B", "second inserted child should be preserved");
 
  assert_equals(a.parentNode, parent, "moved child should now be parented by the new parent");
  assert_equals(b.parentNode, parent, "moved child should now be parented by the new parent");
}, "Node.appendChild(DocumentFragment) inserts fragment children and empties fragment");
