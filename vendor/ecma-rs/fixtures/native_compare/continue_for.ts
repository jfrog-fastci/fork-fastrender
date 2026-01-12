let sum: number = 0;
for (let i: number = 0; i < 5; i = i + 1) {
  if (i === 2) continue;
  sum = sum + i;
}
console.log(sum);
