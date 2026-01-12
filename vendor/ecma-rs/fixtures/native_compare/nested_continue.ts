let outer: number = 0;
let inner: number = 0;
let sum: number = 0;
while (outer < 3) {
  inner = 0;
  while (inner < 3) {
    inner = inner + 1;
    if (inner === 2) continue;
    sum = sum + outer * 10 + inner;
  }
  outer = outer + 1;
}
console.log(sum);
