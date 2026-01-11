// EXPECT: 0
globalThis.__native_result = "pending";

// `Promise.all([])` resolves to an empty array.
Promise.all([]).then((values) => {
  globalThis.__native_result = values.length;
});
