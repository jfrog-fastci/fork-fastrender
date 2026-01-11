// Promise.all preserves input order even when individual promises resolve immediately.
Promise.all([Promise.resolve("a"), Promise.resolve("b")]).then(function (xs) {
  return xs.join("");
})
