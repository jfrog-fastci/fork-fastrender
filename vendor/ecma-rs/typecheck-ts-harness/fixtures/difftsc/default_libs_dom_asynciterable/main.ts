// @target: ES2018

// No `// @lib:` directive: rely on TypeScript's default lib selection.
declare const stream: ReadableStream<number>;

export async function f() {
  for await (const x of stream) {
    x;
  }
}
