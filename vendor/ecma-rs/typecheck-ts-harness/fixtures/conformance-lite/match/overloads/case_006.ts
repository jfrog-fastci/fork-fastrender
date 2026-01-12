// @lib: es5

function g(x: number): "n";
function g(x: string): "s";
function g(x: string | number) {
  return 0 as any;
}

const a: "n" = g(6);
const b: "s" = g("x");
void a;
void b;
