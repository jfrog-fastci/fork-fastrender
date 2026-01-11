// EXPECT: yes
globalThis.__native_result = "pending";

// If `onFinally` is not callable, Promise.prototype.finally behaves like `then(undefined, undefined)`
// and should pass the value through unchanged.
Promise.resolve("yes")
  .finally(null)
  .then((v) => {
    globalThis.__native_result = v;
  });
