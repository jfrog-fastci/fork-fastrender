async_test((t) => {
  const log = [];
  queueMicrotask(() => {
    log.push("microtask");
  });
  setTimeout(
    t.step_func_done(() => {
      log.push("timeout");
      assert_equals(log.join(","), "microtask,timeout");
    }),
    0,
  );
}, "queueMicrotask runs before setTimeout(0)");

