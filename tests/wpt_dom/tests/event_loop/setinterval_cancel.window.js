async_test((t) => {
  let count = 0;
  let id = null;
  id = setInterval(
    t.step_func(() => {
      count++;
      if (count === 3) {
        clearInterval(id);
        setTimeout(
          t.step_func_done(() => {
            assert_equals(count, 3);
          }),
          0,
        );
        return;
      }
      if (count > 3) {
        assert_unreached("interval fired after clearInterval");
      }
    }),
    0,
  );
}, "clearInterval cancels setInterval");

