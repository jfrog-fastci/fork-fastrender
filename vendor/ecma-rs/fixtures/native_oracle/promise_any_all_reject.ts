// EXPECT: AggregateError:2
globalThis.__native_result = "pending";
Promise.any([Promise.reject("a"), Promise.reject("b")]).then(
  () => {
    globalThis.__native_result = "fulfilled";
  },
  (err) => {
    globalThis.__native_result = err.name + ":" + err.errors.length;
  }
);
