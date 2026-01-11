// EXPECT: boom
globalThis.__native_result = "pending";

// If the Promise.try callback throws, the returned promise should reject.
Promise.try(() => {
  throw "boom";
}).then(
  () => {
    globalThis.__native_result = "fulfilled";
  },
  (err) => {
    globalThis.__native_result = err;
  }
);
