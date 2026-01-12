let outer: number = 0;
let inner: number = 0;
let sum: number = 0;
while (outer < 3) {
  inner = 0;
  while (true) {
    if (inner === 2) break;
    sum = sum + outer * 10 + inner;
    inner = inner + 1;
  }
  outer = outer + 1;
}
console.log(sum);
