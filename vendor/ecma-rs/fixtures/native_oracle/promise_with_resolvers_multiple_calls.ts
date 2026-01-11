// EXPECT: a
globalThis.__native_result = "pending";

// Resolve should win over later reject calls (resolving functions are idempotent).
const r = Promise.withResolvers();
r.promise.then(
  (v: string) => {
    globalThis.__native_result = v;
  },
  (err: string) => {
    globalThis.__native_result = "rejected:" + err;
  }
);
r.resolve("a");
r.reject("b");
