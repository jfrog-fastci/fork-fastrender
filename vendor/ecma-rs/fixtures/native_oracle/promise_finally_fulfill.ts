// EXPECT: yes|finally
globalThis.__native_result = "pending";

// Promise.prototype.finally should run before chained `.then` and pass through the original value.
Promise.resolve("yes")
  .finally(() => {
    globalThis.__native_result = "finally";
  })
  .then((v) => {
    globalThis.__native_result = v + "|" + globalThis.__native_result;
  });
