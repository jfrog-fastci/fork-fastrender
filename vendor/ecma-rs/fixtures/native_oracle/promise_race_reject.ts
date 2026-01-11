// EXPECT: boom
globalThis.__native_result = "pending";

// Promise.race should reject if the first settled element rejects.
let resolveA = (_v: string) => {};
let rejectB = (_v: string) => {};
const p1 = new Promise<string>((resolve) => {
  resolveA = resolve;
});
const p2 = new Promise<string>((_, reject) => {
  rejectB = reject;
});

Promise.race([p1, p2]).then(
  (v) => {
    globalThis.__native_result = v;
  },
  (err) => {
    globalThis.__native_result = err;
  }
);

// Reject second promise first; later fulfillment must not change the result.
rejectB("boom");
resolveA("a");
