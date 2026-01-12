// Returns a Promise<string> that uses `await` inside a `for..of` loop body.
(async () => {
  let out = "";
  for (var x of ["a", "b"]) {
    out = out + await Promise.resolve(x);
  }
  return out;
})()
