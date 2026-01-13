// META: script=/resources/testharness.js

test(() => {
  assert_true(typeof DocumentFragment.prototype.querySelector === "function");
  assert_true(typeof DocumentFragment.prototype.querySelectorAll === "function");
  assert_true(
    Object.prototype.hasOwnProperty.call(
      DocumentFragment.prototype,
      "querySelector"
    )
  );
  assert_true(
    Object.prototype.hasOwnProperty.call(
      DocumentFragment.prototype,
      "querySelectorAll"
    )
  );

  const frag = document.createDocumentFragment();
  assert_false(Object.prototype.hasOwnProperty.call(frag, "querySelector"));
  assert_false(Object.prototype.hasOwnProperty.call(frag, "querySelectorAll"));

  assert_true(typeof ShadowRoot.prototype.querySelector === "function");
  assert_true(typeof ShadowRoot.prototype.querySelectorAll === "function");
  assert_true(
    Object.prototype.hasOwnProperty.call(ShadowRoot.prototype, "querySelector")
  );
  assert_true(
    Object.prototype.hasOwnProperty.call(
      ShadowRoot.prototype,
      "querySelectorAll"
    )
  );
}, "DocumentFragment/ShadowRoot expose querySelector(All) on their interface prototypes");

test(() => {
  const frag = document.createDocumentFragment();

  const a = document.createElement("span");
  a.id = "a";
  a.className = "x";
  frag.appendChild(a);

  const b = document.createElement("div");
  b.className = "x";
  frag.appendChild(b);

  assert_equals(frag.querySelector("#a"), a);

  const matches = frag.querySelectorAll(".x");
  assert_equals(matches.length, 2);
  assert_equals(matches[0], a);
  assert_equals(matches[1], b);
}, "DocumentFragment.querySelector(All) searches within the fragment subtree");

test(() => {
  const frag = document.createDocumentFragment();
  const a = document.createElement("span");
  frag.appendChild(a);
  assert_equals(frag.querySelector(":scope > span"), a);
}, "DocumentFragment.querySelector supports :scope child combinators");

test(() => {
  const frag = document.createDocumentFragment();
  let threw = false;
  let name = "";
  try {
    frag.querySelector("div[");
  } catch (e) {
    threw = true;
    name = e.name;
  }
  assert_true(threw);
  assert_equals(name, "SyntaxError");
}, "DocumentFragment.querySelector throws SyntaxError for invalid selectors");
