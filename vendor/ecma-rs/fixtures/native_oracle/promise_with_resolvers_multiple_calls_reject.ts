// EXPECT: b
globalThis.__native_result = "pending";

// Reject should win over later resolve calls (resolving functions are idempotent).
const r = Promise.withResolvers();
r.promise.then(
  (v: string) => {
    globalThis.__native_result = "fulfilled:" + v;
  },
  (err: string) => {
    globalThis.__native_result = err;
  }
);
r.reject("b");
r.resolve("a");
