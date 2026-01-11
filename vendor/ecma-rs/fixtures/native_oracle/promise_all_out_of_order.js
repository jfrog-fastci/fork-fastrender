// Promise.all preserves input order even when promises resolve out-of-order.
var resolveA;
var resolveB;
var p1 = new Promise(function (resolve) {
  resolveA = resolve;
});
var p2 = new Promise(function (resolve) {
  resolveB = resolve;
});
var all = Promise.all([p1, p2]).then(function (xs) {
  return xs.join("");
});
// Resolve in the opposite order to ensure Promise.all uses input ordering.
resolveB("b");
resolveA("a");
all
