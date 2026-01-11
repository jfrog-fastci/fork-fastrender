// EXPECT: nope
globalThis.__native_result = "pending";

const r = Promise.withResolvers();
r.promise.then(
  () => {
    globalThis.__native_result = "fulfilled";
  },
  (err: string) => {
    globalThis.__native_result = err;
  }
);
r.reject("nope");
