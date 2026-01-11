// EXPECT: 3
globalThis.__native_result = "pending";

// Promise.try should call the callback immediately and resolve the returned promise with the
// callback's return value.
Promise.try((a: number, b: number) => a + b, 1, 2).then((v) => {
  globalThis.__native_result = v;
});
