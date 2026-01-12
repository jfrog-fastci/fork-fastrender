function run(): number {
  let sum: number = 0;
  for (let i: number = 0; i < 5; i = i + 1) {
    sum = sum + i;
  }
  return sum;
}

console.log(run());
