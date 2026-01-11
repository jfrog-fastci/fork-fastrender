// EXPECT: b
globalThis.__native_result = "pending";

// Promise.all should reject as soon as one element rejects.
let resolveA = (_v: string) => {};
let rejectB = (_v: string) => {};
const p1 = new Promise<string>((resolve) => {
  resolveA = resolve;
});
const p2 = new Promise<string>((_, reject) => {
  rejectB = reject;
});

Promise.all([p1, p2]).then(
  () => {
    globalThis.__native_result = "fulfilled";
  },
  (err) => {
    globalThis.__native_result = err;
  }
);

// Reject second promise first; later fulfillment of the first promise must not change the result.
rejectB("b");
resolveA("a");
