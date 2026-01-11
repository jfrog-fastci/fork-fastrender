// EXPECT: no|finally
globalThis.__native_result = "pending";

// Promise.prototype.finally should run on rejection and preserve the rejection reason.
Promise.reject("no")
  .finally(() => {
    globalThis.__native_result = "finally";
  })
  .then(
    () => {
      globalThis.__native_result = "fulfilled";
    },
    (err) => {
      globalThis.__native_result = err + "|" + globalThis.__native_result;
    }
  );
