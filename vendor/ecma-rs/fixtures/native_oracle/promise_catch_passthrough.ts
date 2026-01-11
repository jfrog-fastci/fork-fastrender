// EXPECT: yes
globalThis.__native_result = "pending";

// Promise.prototype.catch should not affect fulfilled promises.
Promise.resolve("yes")
  .catch(() => "no")
  .then((v) => {
    globalThis.__native_result = v;
  });
