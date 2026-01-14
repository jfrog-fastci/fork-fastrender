// Async/await + Promise jobs + microtask checkpoint coverage.
let side = "";
async function f(x) {
  side += "a";
  try {
    for (let i = 0; i < 3; i++) {
      x = await Promise.resolve(x + i);
      side += "b";
    }
    return x;
  } finally {
    side += "f";
  }
}
Promise.resolve(1).then(async (v) => {
  const r = await f(v);
  side += ":" + r;
});
Promise.resolve().then(() => (side += "|"));
side;

