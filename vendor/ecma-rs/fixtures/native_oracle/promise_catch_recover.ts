// EXPECT: ok
globalThis.__native_result = "pending";

// Promise.prototype.catch should handle rejections and allow recovery by returning a value.
Promise.reject("no")
  .catch(() => "ok")
  .then((v) => {
    globalThis.__native_result = v;
  });
