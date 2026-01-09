test(() => {
  const root = new EventTarget();
  const parent = new EventTarget(root);
  const target = new EventTarget(parent);

  const log = [];

  root.addEventListener("x", () => log.push("root-capture"), { capture: true });
  parent.addEventListener("x", () => log.push("parent-capture"), {
    capture: true,
  });
  target.addEventListener("x", () => log.push("target-capture"), {
    capture: true,
  });

  target.addEventListener("x", () => log.push("target-bubble"));
  parent.addEventListener("x", () => log.push("parent-bubble"));
  root.addEventListener("x", () => log.push("root-bubble"));

  target.dispatchEvent(new Event("x", { bubbles: true }));
  assert_equals(
    log.join(","),
    "root-capture,parent-capture,target-capture,target-bubble,parent-bubble,root-bubble",
  );
}, "EventTarget capture/target/bubble ordering");

test(() => {
  const root = new EventTarget();
  const parent = new EventTarget(root);
  const target = new EventTarget(parent);

  const log = [];
  parent.addEventListener("x", (e) => {
    log.push("parent");
    e.stopPropagation();
  });
  root.addEventListener("x", () => log.push("root"));

  target.dispatchEvent(new Event("x", { bubbles: true }));
  assert_equals(log.join(","), "parent");
}, "stopPropagation stops propagation to later targets");

test(() => {
  const root = new EventTarget();
  const parent = new EventTarget(root);
  const target = new EventTarget(parent);

  const log = [];
  target.addEventListener("x", (e) => {
    log.push("first");
    e.stopImmediatePropagation();
  });
  target.addEventListener("x", () => log.push("second"));
  parent.addEventListener("x", () => log.push("parent"));

  target.dispatchEvent(new Event("x", { bubbles: true }));
  assert_equals(log.join(","), "first");
}, "stopImmediatePropagation stops other listeners and propagation");

