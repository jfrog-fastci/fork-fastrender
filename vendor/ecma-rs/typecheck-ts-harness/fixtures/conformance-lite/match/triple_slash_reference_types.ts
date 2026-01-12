// @lib: es5

// @filename: main.ts
/// <reference types="example" />

export const v: string = exampleValue;

// @filename: node_modules/@types/example/index.d.ts
declare const exampleValue: string;
