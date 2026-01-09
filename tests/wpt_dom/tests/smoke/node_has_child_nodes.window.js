test(() => {
  assert_true(document.hasChildNodes(), "document should have initial child nodes");
}, "Document.hasChildNodes");

test(() => {
  const host = document.createElement("div");
  assert_false(host.hasChildNodes(), "new element should have no children");

  const child = document.createElement("span");
  host.appendChild(child);
  assert_true(host.hasChildNodes(), "element with an appended child should report children");

  host.removeChild(child);
  assert_false(host.hasChildNodes(), "element should report no children after removal");
}, "Element.hasChildNodes reflects DOM mutations");

test(() => {
  const frag = document.createDocumentFragment();
  assert_false(frag.hasChildNodes(), "new fragment should have no children");

  const child = document.createElement("span");
  frag.appendChild(child);
  assert_true(frag.hasChildNodes(), "fragment should report children after appendChild");

  document.body.appendChild(frag);
  assert_false(frag.hasChildNodes(), "fragment insertion should empty the fragment");
}, "DocumentFragment.hasChildNodes with insertion semantics");

