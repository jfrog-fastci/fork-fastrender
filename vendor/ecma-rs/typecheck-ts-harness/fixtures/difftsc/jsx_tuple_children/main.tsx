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

// Variadic tuple-typed children.
const variadicPass = <p>{1}{"a"}{"b"}</p>;
const variadicFail = <p>{1}{2}</p>;

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
