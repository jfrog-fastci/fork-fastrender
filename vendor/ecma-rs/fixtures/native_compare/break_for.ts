let sum: number = 0;
for (let i: number = 0; i < 10; i = i + 1) {
  if (i === 3) break;
  sum = sum + i;
}
console.log(sum);
