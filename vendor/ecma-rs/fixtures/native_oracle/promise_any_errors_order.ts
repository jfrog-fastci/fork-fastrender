// EXPECT: ab
globalThis.__native_result = "pending";

// Promise.any should reject with AggregateError whose `errors` preserve input ordering.
Promise.any([Promise.reject("a"), Promise.reject("b")]).then(
  () => {
    globalThis.__native_result = "fulfilled";
  },
  (err) => {
    globalThis.__native_result = err.errors[0] + err.errors[1];
  }
);
