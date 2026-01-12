// @lib: es5

function map<T, U>(items: T[], fn: (value: T, index: number) => U): U[] {
  return [] as any;
}

const numbers = [1, 2, 3];
export const strings = map(numbers, (n) => n.toFixed());
export const doubled = map(numbers, (n, idx) => {
  return n + idx;
});

function takesCallback(cb: (value: string) => void) {}

takesCallback((value) => {
  const upper = value.toUpperCase();
  upper.toLowerCase();
});
