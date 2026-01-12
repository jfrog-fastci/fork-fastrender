// @lib: es5
const globalThis = {
  eval: (_src: string) => 0,
};

globalThis["eval"]("1 + 2");
