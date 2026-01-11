// EXPECT: boom
globalThis.__native_result = "pending";

// If `finally` throws, it must override a prior fulfillment.
Promise.resolve("ok")
  .finally(() => {
    throw "boom";
  })
  .then(
    () => {
      globalThis.__native_result = "fulfilled";
    },
    (err) => {
      globalThis.__native_result = err;
    }
  );
