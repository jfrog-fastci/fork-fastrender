function run(): number {
  let i: number = 0;
  let sum: number = 0;
  do {
    sum = sum + i;
    i = i + 1;
  } while (i < 5);
  return sum;
}

console.log(run());
