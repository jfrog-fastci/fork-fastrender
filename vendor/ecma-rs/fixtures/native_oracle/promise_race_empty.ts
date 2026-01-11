// EXPECT: pending
globalThis.__native_result = "pending";

// `Promise.race([])` returns a promise that never settles.
Promise.race([]).then(() => {
  globalThis.__native_result = "settled";
});
