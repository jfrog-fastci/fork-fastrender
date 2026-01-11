// EXPECT: 3
globalThis.__native_result = "pending";
Promise.resolve(1)
  .then((v) => v + 1)
  .then((v) => {
    globalThis.__native_result = v + 1;
  });
