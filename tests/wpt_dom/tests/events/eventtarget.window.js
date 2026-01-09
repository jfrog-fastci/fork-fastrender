// META: script=/resources/testharness.js

test(() => {
  const calls = [];

  const parent = document.createElement("div");
  const child = document.createElement("span");

  // Attach the subtree to the document so the event path is:
  // Window -> Document -> parent -> child.
  document.appendChild(parent);
  parent.appendChild(child);

  parent.addEventListener("x", () => calls.push("parent_capture"), true);
  parent.addEventListener("x", () => calls.push("parent_bubble"));

  child.addEventListener("x", () => calls.push("child_capture"), { capture: true });
  child.addEventListener("x", () => calls.push("child_bubble"));

  const ev = new Event("x", { bubbles: true });
  const ok = child.dispatchEvent(ev);
  assert_true(ok, "dispatchEvent should return true when not canceled");
  assert_equals(
    calls.join(","),
    "parent_capture,child_capture,child_bubble,parent_bubble",
    "expected capture listeners to run before bubbling listeners"
  );
}, "EventTarget capture/bubble ordering");

test(() => {
  const el = document.createElement("div");
  let saw = false;

  el.addEventListener("x", (e) => {
    saw = true;
    e.preventDefault();
  });

  const ev = new Event("x", { cancelable: true });
  const ok = el.dispatchEvent(ev);

  assert_true(saw, "listener should have run");
  assert_true(ev.defaultPrevented, "preventDefault should set defaultPrevented for cancelable events");
  assert_false(ok, "dispatchEvent should return false when default was prevented");
}, "Event.preventDefault sets defaultPrevented for cancelable events");

test(() => {
  const el = document.createElement("div");
  let called = 0;
  function cb() {
    called++;
  }

  el.addEventListener("x", cb);
  el.removeEventListener("x", cb);
  el.dispatchEvent(new Event("x"));
  assert_equals(called, 0, "removed listener should not be invoked");
}, "removeEventListener prevents invocation");

test(() => {
  const el = document.createElement("div");
  let called = 0;
  function cb() {
    called++;
  }

  el.addEventListener("x", cb);
  el.addEventListener("x", cb);
  el.dispatchEvent(new Event("x"));
  assert_equals(called, 1, "duplicate addEventListener registrations should be ignored");
}, "addEventListener does not register duplicates");

