// Regression/hang guardrail: an infinite microtask chain should be terminated by the VM budget,
// not hang the fuzz process.
let n = 0;
function loop() {
  n++;
  return Promise.resolve().then(loop);
}
Promise.resolve().then(loop);
n;

