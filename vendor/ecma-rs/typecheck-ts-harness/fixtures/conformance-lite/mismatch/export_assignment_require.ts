// @lib: es5

// @filename: dep.ts
const foo: 1 = 1;
export = foo;

// @filename: main.ts
import foo = require("./dep");

export const x: 1 = foo;
