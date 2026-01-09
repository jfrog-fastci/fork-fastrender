test(() => {
  const target = new EventTarget();
  let ran = false;
  target.addEventListener(
    "x",
    (e) => {
      ran = true;
      e.preventDefault();
    },
    { passive: true },
  );

  const e = new Event("x", { cancelable: true });
  const res = target.dispatchEvent(e);
  assert_true(ran);
  assert_false(e.defaultPrevented);
  assert_true(res);
}, "passive listeners cannot set defaultPrevented");

