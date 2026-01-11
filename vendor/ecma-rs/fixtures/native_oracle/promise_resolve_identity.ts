// EXPECT: true

// `Promise.resolve(p)` returns `p` when `p` is already a Promise of the same constructor.
const p = Promise.resolve("ok");
globalThis.__native_result = Promise.resolve(p) === p;
