// @jsx: react
// @strict: true

// Passing tuple-typed children.
const pass = <div>{1}{"x"}</div>;

// Failing tuple-typed children (mismatched order).
const fail = (
  <div>
    {"x"}
    {1}
  </div>
);

// Per-index contextual typing (strict includes noImplicitAny).
const contextual = (
  <span>
    {(ev) => {
      ev.x;
    }}
    {(ev) => {
      ev.y;
    }}
  </span>
);

