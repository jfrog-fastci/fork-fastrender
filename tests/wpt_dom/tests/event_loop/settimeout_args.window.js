async_test((t) => {
  const obj = { ok: true };
  setTimeout(
    t.step_func_done((a, b, c) => {
      assert_equals(a, 1);
      assert_equals(b, "two");
      assert_equals(c, obj);
    }),
    0,
    1,
    "two",
    obj,
  );
}, "setTimeout passes additional arguments to callback");

