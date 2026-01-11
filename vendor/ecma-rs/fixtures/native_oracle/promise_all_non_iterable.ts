// EXPECT: TypeError
globalThis.__native_result = "pending";

// Promise.all(non-iterable) should reject with a TypeError.
Promise.all(123).then(
  () => {
    globalThis.__native_result = "fulfilled";
  },
  (err) => {
    globalThis.__native_result = err.name;
  }
);
