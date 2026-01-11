// EXPECT: AggregateError:0
globalThis.__native_result = "pending";

// `Promise.any([])` should reject with an AggregateError whose `errors` list is empty.
Promise.any([]).then(
  () => {
    globalThis.__native_result = "fulfilled";
  },
  (err) => {
    globalThis.__native_result = err.name + ":" + err.errors.length;
  }
);
