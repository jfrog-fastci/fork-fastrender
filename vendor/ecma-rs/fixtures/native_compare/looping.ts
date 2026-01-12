function run(): number {
  let sum: number = 0;
  let i: number = 0;
  while (i < 10) {
    sum = sum + i;
    i = i + 1;
  }
  return sum;
}

console.log(run());

