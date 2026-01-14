// Top-level await: compiled classic-script async executor should suspend and resume via microtasks.
let x = 0;
await Promise.resolve(1);
Promise.resolve().then(() => (x += 1));
x;

