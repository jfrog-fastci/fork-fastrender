let i: number = 0;
let sum: number = 0;
while (i < 5) {
  i = i + 1;
  if (i === 3) continue;
  sum = sum + i;
}
console.log(sum);
