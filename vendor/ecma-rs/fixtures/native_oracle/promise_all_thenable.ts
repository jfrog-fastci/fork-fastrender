// EXPECT: ab
globalThis.__native_result = "pending";

// `Promise.all` must treat thenables like Promises and preserve *input order* even when they settle
// out-of-order.
const thenable1 = {
  then: (resolve: (v: string) => void) => {
    // Resolve on a later microtask to ensure this thenable settles after `thenable2`.
    Promise.resolve().then(() => resolve("a"));
  },
};
const thenable2 = {
  then: (resolve: (v: string) => void) => {
    resolve("b");
  },
};

Promise.all([thenable1, thenable2]).then((xs) => {
  globalThis.__native_result = xs.join("");
});
