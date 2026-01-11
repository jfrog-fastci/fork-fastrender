# EXEC.md — JavaScript Execution Research & Design

## Meta

- This doc is for discussion, direction, and design exploration—not implementation specs
- Code examples are illustrative, not literal correct implementations
- Written for internal engineers; assumes familiarity with compilers/runtimes
- Living document; update as thinking evolves

Status: early exploration / brainstorming. Nothing committed.

---

## Contents

1. [Philosophy](#philosophy)
2. [Questions](#questions)
3. [TypeScript Types](#typescript-types)
4. [Open vs Closed World](#open-vs-closed-world)
5. [Closed-World Analysis](#closed-world-analysis)
6. [Why Rust is Faster](#why-rust-is-faster)
7. [AOT Approaches](#aot-approaches)
8. [Hybrid Architecture](#hybrid-architecture)
9. [Other Directions](#other-directions)
10. [GC and LLVM](#gc-and-llvm)
11. [LLVM: Ours vs Theirs](#llvm-ours-vs-theirs)
12. [Speedup Sources](#speedup-sources)
13. [Graceful Degradation](#graceful-degradation)
14. [Pipeline](#pipeline)
15. [Analysis Algorithms](#analysis-algorithms)
16. [Open Questions](#open-questions)
17. [JIT vs AOT Comparison](#jit-vs-aot-comparison)
18. [References](#references)

**Related Documents:**
- `EXEC.plan.md` — Concrete implementation plan, optimization catalog, milestones
- `EXEC.discussion.md` — Working notes on compiler architecture, IR design

---

## Philosophy

Three principles:

### 1. "The programmer already told us what they meant. Be smart enough to understand it."

They wrote `number` but they're doing integer arithmetic. We should see that.

They wrote `class Point` but they never extend it. We should see that.

They wrote `for (const x of arr)` but it's a simple array. We should see that.

The programmer writes idiomatic TypeScript, not compiler annotations. Our job: extract everything from what they wrote, plus what they didn't write (didn't extend the class, didn't mutate the object, didn't use dynamic access).

### 2. "Don't make me write Rust. Make my TypeScript fast."

If I wanted explicit control, I'd use a language with explicit control. I'm using TypeScript because I want to think at a higher level. The compiler bridges the gap.

No need for `int`—that's not JS anymore. We should determine at compilation time if something is likely int-only. Be cleverer, do the heavier lifting, don't make the programmer do it.

### 3. "The codebase is the world. Use that."

JITs assume open world because they have to. We don't. The codebase is finite, knowable, closed. Every fact true about the codebase is a potential optimization.

When "anything could happen" in an open system with scripts streaming in, that limits what you can do. Same reason rustc works well: it knows all code at compile time.

### Non-goals

- Not a new language. If you wanted stricter, you'd use Rust.
- Not about banning features. Banning eval is different—serious codebases already avoid it.
- Not about programmer annotations. We detect things like int usage, not ask for them.

### Target

Large production codebases with enough structure/rigor that they may as well have been written in a stricter language. Everything typed, no random property defining, probably no `==`.

---

## Questions

Three interrelated questions this project explores:

1. Does type information enable meaningfully faster code? Better optimizations, native compilation?

2. Can you build an AOT compiler (output is binary)?

3. Can you build a JS engine that leverages types while cooperating with untyped code? Native code alongside regular JS/VM functions?

---

## TypeScript Types

### Limited Optimization Potential

TypeScript's types provide surprisingly little optimization potential out of the box.

TypeScript chose ergonomics over soundness:

```typescript
const arr: number[] = [1, 2, 3];
(arr as any).push("oops");
// arr is still typed as number[], but contains a string
```

An AOT compiler can't trust types without runtime checks. Compare to Rust where `Vec<i64>` guarantees memory layout.

Even with perfect types, JS semantics impose constraints:

Numbers are IEEE 754 doubles (with SMI optimization in V8 for 31-bit ints). `x: number` doesn't tell you:
- SMI (unboxed, fast arithmetic)
- Heap number (boxed, slower)
- NaN, Infinity, -0 (special cases)

Objects are property bags with prototype chains. `interface Point { x: number; y: number }` doesn't guarantee layout. Could have:
- Extra properties
- Properties on prototype
- Getters/setters
- `Object.defineProperty` modifications

### Where Types Help

Escape analysis and shape prediction: if you prove a function only receives objects of a specific shape (V8's hidden class / SpiderMonkey's Shape), you can:
- Pre-generate IC entries
- Skip polymorphic dispatch
- Inline property access as direct offset loads

Devirtualization: knowing concrete type enables direct calls instead of prototype lookups:

```typescript
class Vec3 { 
    dot(other: Vec3): number { /* ... */ } 
}
// If we know v1 and v2 are exactly Vec3 (not subclasses):
v1.dot(v2) // Direct call, not prototype lookup
```

Monomorphization: generate specialized code per type instantiation. Requires whole-program analysis, explodes code size.

### JITs Already Do This

V8's TurboFan, SpiderMonkey's Warp, JSC's DFG/FTL do speculative optimization based on runtime type feedback. They observe actual types and optimize accordingly.

Does static type info provide signal the JIT doesn't have?

Usually no. The JIT observes `point.x` always receives objects with shape `{x: double, y: double}` at offset 16. Your `Point` annotation doesn't add information.

Where static types might help: cold code (not yet profiled), ahead-of-time scenarios (can't afford warmup).

### JIT Capabilities

Modern JITs aren't primitive:
- Sea of Nodes IR (TurboFan)—same as HotSpot C2
- GVN, LICM, escape analysis
- Aggressive inlining with polymorphic ICs
- Range analysis for bounds check elimination
- Redundancy elimination

Speculation is a feature: optimize for what actually happens. `(x: number | string)` that only receives numbers gets integer-optimized code.

### Gaps

| JIT Limitation | Static Types Advantage |
|----------------|------------------------|
| Warmup time | AOT is fast from first call |
| Per-function optimization | Whole-program sees more |
| Deopt guards everywhere | Proven types = no guards |
| Can't trust declared types | Closed world = types are truth |
| Megamorphism | Monomorphization eliminates dispatch |

---

## Open vs Closed World

This is the core leverage point.

### Open World (JITs)

```
Code loads → Execute → More code might load → Execute → ...
```

At any moment, new code could:
- Subclass your class
- Add properties to your object
- Call your function with unexpected types
- Patch prototypes

JIT must be defensive. Every optimization is speculative, guarded, revocable.

### Closed World (AOT)

```
All code known → Analyze everything → Compile once → Execute
```

How rustc, GHC, MLton work. They do optimizations JITs fundamentally cannot.

Core difference: JIT optimizations are revocable; AOT optimizations are permanent.

AOT can:
- Eliminate guards entirely (not just make them cheaper)
- Make stronger inlining decisions (no fear of invalidation)
- Lay out memory more aggressively

---

## Closed-World Analysis

### Class Sealing (Without Declaring sealed)

```typescript
// point.ts
class Point {
    constructor(public x: number, public y: number) {}
    magnitude(): number {
        return Math.sqrt(this.x * this.x + this.y * this.y);
    }
}

// main.ts
const p = new Point(3, 4);
console.log(p.magnitude());
```

JIT: `magnitude()` could be overridden. Must check prototype or guard against subclass.

Whole-program: Scan codebase. Point ever extended? No. Every `Point.magnitude()` call is direct:

```llvm
define double @Point_magnitude(%Point* %this) {
    %x = load double, ptr getelementptr(%Point, %this, 0, 0)
    %y = load double, ptr getelementptr(%Point, %this, 0, 1)
    %x2 = fmul double %x, %x
    %y2 = fmul double %y, %y
    %sum = fadd double %x2, %y2
    %result = call double @llvm.sqrt.f64(double %sum)
    ret double %result
}
```

No guards, no dispatch. Just math.

### Shape Inference (Without Annotations)

```typescript
function createUser(name: string, age: number) {
    return { name, age, active: true };
}

const users = data.map(d => createUser(d.name, d.age));
```

JIT: Objects might have properties added later. Maintain IC, handle transitions.

Whole-program: Trace all uses of objects from `createUser`. Properties ever added? Deleted? If not, shape is final:

```
Shape_User = { name: string @ offset 8, age: f64 @ offset 16, active: bool @ offset 24 }
```

Property access compiles to direct offset load. No IC, no shape check.

### Type Narrowing Beyond TS

TypeScript infers types but is conservative and local. Whole-program can infer tighter types:

```typescript
function process(x: number | string) {
    return compute(x);
}

// All call sites:
process(1);
process(2);
process(someNumber);
// No string calls anywhere!
```

Whole-program: `process` only ever receives `number`. Generate code assuming number, no guards.

### Integer Detection

Don't make programmers write `int`. Detect it:

```typescript
function factorial(n: number): number {
    if (n <= 1) return 1;
    return n * factorial(n - 1);
}

// Call sites:
factorial(5);
factorial(10);
factorial(Math.floor(x));
```

Analysis:
- All calls pass integers
- Operations are `<=`, `-`, `*` on integers
- Result is always integer (within range)

Generate integer-specialized version:

```llvm
define i64 @factorial_int(i64 %n) {
entry:
    %cond = icmp sle i64 %n, 1
    br i1 %cond, label %base, label %recurse
base:
    ret i64 1
recurse:
    %n_minus_1 = sub i64 %n, 1
    %sub_result = call i64 @factorial_int(i64 %n_minus_1)
    %result = mul i64 %n, %sub_result
    ret i64 %result
}
```

Native integer arithmetic. No tagging, no SMI overflow checks.

### Cross-Module Escape Analysis

```typescript
function dotProduct(a: Point, b: Point): number {
    return a.x * b.x + a.y * b.y;
}

function computeScore(data: number[]): number {
    let sum = 0;
    for (let i = 0; i < data.length; i += 4) {
        const p1 = new Point(data[i], data[i+1]);      // allocation
        const p2 = new Point(data[i+2], data[i+3]);    // allocation
        sum += dotProduct(p1, p2);
    }
    return sum;
}
```

JIT: Might do escape analysis within function, but cross-function is limited.

Whole-program: Trace p1 and p2. Escape? Address taken? Stored in data structure? No. Scalar replacement:

```llvm
define double @computeScore(ptr %data, i64 %len) {
    ; No allocations. Points decomposed to registers.
    %p1_x = load double, ptr %data[i]
    %p1_y = load double, ptr %data[i+1]
    %p2_x = load double, ptr %data[i+2]
    %p2_y = load double, ptr %data[i+3]
    %dot = fadd double (fmul %p1_x, %p2_x), (fmul %p1_y, %p2_y)
    ...
}
```

Zero allocations in hot loop. Point objects never exist at runtime.

---

## Why Rust is Faster

Types matter, but Rust's speed comes from several factors:

### Known Memory Layout

```rust
struct Point { x: f64, y: f64 }  // Exactly 16 bytes, fields at offset 0 and 8
```

```typescript
interface Point { x: number; y: number }
// Could be:
// - Inline properties at fixed offsets (fast)
// - Dictionary-mode properties (slow)  
// - Properties on prototype (slower)
// - Getters (arbitrary code execution)
```

### No Boxing

```rust
let nums: Vec<i64> = vec![1, 2, 3];
// Memory: [capacity, length, ptr] -> [1, 2, 3] contiguous i64s
```

```javascript
const nums: number[] = [1, 2, 3];
// Memory: [shape, length, elements_ptr] -> [tagged_1, tagged_2, tagged_3]
// Each element potentially boxed or SMI-tagged
```

### No Aliasing Surprises

Rust's borrow checker provides optimization info TypeScript can never provide:

```rust
fn process(a: &mut Vec<i32>, b: &mut Vec<i32>) {
    // Compiler knows a and b don't alias
    // Can parallelize, reorder, vectorize freely
}
```

```typescript
function process(a: number[], b: number[]) {
    // a and b might be the same array
    a[0] = 1;
    b[0] = 2;  // Might overwrite a[0]
    return a[0];  // Can't optimize to "return 1"
}
```

Unfixable in JS semantics. You'd need `restrict`-style annotations or borrow-checking, at which point you're not writing JS anymore.

### Monomorphization

```rust
fn add<T: Add>(a: T, b: T) -> T { a + b }
// Compiler generates: add_i32, add_i64, add_f32, add_f64, ...
```

```typescript
function add<T extends number>(a: T, b: T): T { return a + b as T; }
// TypeScript erases T at runtime - it's just "add(a, b)"
```

---

## AOT Approaches

### Approach A: Strict Subset (AssemblyScript)

AssemblyScript compiles TypeScript-like to WASM, but it's not TypeScript:
- No structural typing (nominal only)
- No union types (mostly)
- Explicit memory management or linear GC
- Fixed-size integers (`i32`, `i64`, `f32`, `f64`)

Compilation is straightforward because the language maps cleanly to WASM types.

Downside: lose JS interop and TS expressiveness.

### Approach B: Whole-Program AOT with Speculation

```
TypeScript Source
       ↓
   Type Analysis (tsc or custom)
       ↓
   IR Generation (typed SSA form)
       ↓
   Shape Analysis / Escape Analysis
       ↓
   Speculative Native Code Generation
       ↓
   LLVM IR (with guards + deopt points)
       ↓
   Native Binary + Deopt Metadata
```

LLVM concepts:

Statepoints for GC: LLVM has `gc.statepoint` and `gc.relocate` intrinsics for precise stack maps:

> Note (LLVM 18 / opaque pointers): the verifier requirements are subtle
> (mandatory trailing `i32 0, i32 0`, `elementtype(...)` on the callee operand,
> GC pointers in `addrspace(1)` for `gc "coreclr"`, etc). See:
> - `vendor/ecma-rs/docs/llvm_statepoints_llvm18.md`
> - `vendor/ecma-rs/fixtures/llvm_stackmap_abi/statepoint.ll`

```llvm
%statepoint_token = call token (i64, i32, ptr, i32, i32, ...)
    @llvm.experimental.gc.statepoint.p0(
        i64 0, i32 0,
        ptr elementtype(void (ptr addrspace(1), ptr addrspace(1))) @some_function,
        i32 2, i32 0,
        ptr addrspace(1) %obj1, ptr addrspace(1) %obj2,
        i32 0, i32 0) ; num_transition_args, num_deopt_args
    [ "gc-live"(ptr addrspace(1) %obj1, ptr addrspace(1) %obj2) ]
%obj1.relocated = call ptr addrspace(1) @llvm.experimental.gc.relocate.p1(
    token %statepoint_token, i32 0, i32 0)
```

**LLVM 18 note:** In textual IR, statepoints require `ptr elementtype(<fn sig>) @callee` (opaque pointers),
and the varargs list must end with two *constant* `i32`s: `num_transition_args` and `num_deopt_args` (usually `0, 0`).
Inline transition args are deprecated; always use `num_transition_args=0`.

Tells LLVM where GC roots are so it generates stack maps.

Deoptimization via patchpoints: `@llvm.experimental.patchpoint` reserves space for call sites, records live values, patches in jumps to deopt code at runtime.

Object representation options:
1. NaN boxing: pack values into 64-bit doubles, use NaN space for pointers/tags
2. Tagged pointers: use low bits for type tags (like V8's SMI tagging)
3. Uniform boxing: everything is pointer to heap object (simpler, slower)

V8 uses pointer tagging where SMIs have low bit clear, pointers have it set:
```
SMI:    [31-bit integer value][0]
Pointer:[heap address       ][1]
```

Hidden classes / shapes:

```c
struct Shape {
    Shape* parent;           // for prototype chain
    PropertyDescriptor* properties;
    uint32_t property_count;
    uint32_t instance_size;
};

struct JSObject {
    Shape* shape;
    Value* slots;  // property values, layout described by shape
};
```

### Approach C: Porffor-Style

Porffor's approach:
- Parse TypeScript, preserve type annotations
- Generate bytecode for custom VM
- Compile bytecode to WASM or native via LLVM
- Use type info to specialize operations

Challenges:
- Spec compliance: JS has many edge cases (ToPrimitive, ToNumber, exotic objects)
- Builtins: `Array.prototype.map`, `String.prototype.split` are complex
- Code size: full JS semantics generate lots of code per operation

### The eval() Problem

Complete JS must handle:

```javascript
eval("obj." + propName + " = " + value);
new Function("return " + expr)();
```

Incompatible with pure AOT. Options:
1. Ship interpreter/JIT alongside AOT code (hybrid)
2. Forbid dynamic code generation (break spec)
3. Include full parser/compiler in runtime

---

## Hybrid Architecture

### Tiered Execution

```
                    ┌─────────────────────────────────┐
                    │         TypeScript Source        │
                    └──────────────┬──────────────────┘
                                   ↓
                    ┌─────────────────────────────────┐
                    │   Type Analysis & IR Generation  │
                    │   (preserve type annotations)    │
                    └──────────────┬──────────────────┘
                                   ↓
        ┌──────────────────────────┴───────────────────────┐
        ↓                                                   ↓
┌───────────────────┐                         ┌────────────────────────┐
│ Typed Code Paths  │                         │  Untyped/Dynamic Code  │
│ (type-certain)    │                         │  (uncertain types)     │
└────────┬──────────┘                         └──────────┬─────────────┘
         ↓                                               ↓
┌───────────────────┐                         ┌────────────────────────┐
│  Native Code Gen  │                         │   Interpreter/JIT      │
│  (LLVM backend)   │                         │   (traditional V8-style)│
│  - No type guards │                         │   - Type feedback      │
│  - Direct calls   │                         │   - Inline caches      │
│  - Unboxed values │                         │   - Speculative opt    │
└────────┬──────────┘                         └──────────┬─────────────┘
         │                                               │
         └────────────────────┬──────────────────────────┘
                              ↓
                    ┌─────────────────────────────────┐
                    │    Boundary Crossing Layer       │
                    │    (boxing/unboxing, guards)     │
                    └─────────────────────────────────┘
```

### Boundary Crossing

When typed code calls untyped (or vice versa), need contracts at boundary.

Typed → Untyped (easy):
```typescript
function processPoint(p: Point): number {
    return externalLibrary.compute(p); // calls untyped code
}
```
Just pass the object. Untyped side uses normal JS semantics.

Untyped → Typed (hard):
```typescript
declare function yourTypedFn(x: number, y: string): Point;
```

Need to validate arguments. Options:
1. Trust and hope (unsafe)
2. Runtime checks (overhead at every call)
3. Wrapper functions that validate then call fast path

### Gradual Typing Research

Research on gradual typing (Siek & Taha, Typed Racket) found: boundary overhead can dominate execution time if frequently crossing typed/untyped.

Typed Racket inserts contract wrappers at module boundaries:
```racket
(define (process-data/contracted lst)
  (unless (and (list? lst) (andmap integer? lst))
    (contract-violation!))
  (process-data lst))
```

Cost: O(n) check for every call with list argument.

### Type-Aware IC Stubs

Instead of purely speculative ICs, pre-seed with type info:

```
Traditional IC (V8-style):
  if (obj->shape == cached_shape_1) goto fast_path_1;
  if (obj->shape == cached_shape_2) goto fast_path_2;
  goto slow_path_megamorphic;

Type-Aware IC:
  // Compiler knows interface Point { x: number; y: number }
  // Pre-generate shapes matching this interface
  if (obj->shape == point_shape_variant_1) goto fast_path;
  if (obj->shape == point_shape_variant_2) goto fast_path;
  goto slow_path_or_deopt;
```

Skip warmup where IC learns shapes. Cold code starts fast.

### GraalJS / Truffle

Truffle: framework for language interpreters that get JIT-compiled via partial evaluation:
- Write AST interpreter in Java
- Annotate specialization points (`@Specialization`)
- Graal partially evaluates interpreter + AST
- Produces optimized machine code

```java
@Specialization(guards = "isInt(left) && isInt(right)")
int doInts(int left, int right) {
    return left + right;
}

@Specialization(replaces = "doInts") 
double doDoubles(double left, double right) {
    return left + right;
}
```

TypeScript types could feed into this: pre-specialize based on declared types instead of discovering at runtime.

### Meta-Tracing

In a tracing JIT (LuaJIT-style), record execution traces and compile them. Types could influence trace selection:

```
Without type info:
  - Record actual execution
  - Discover x is always integer
  - Compile specialized integer trace
  - Guard: if not int, side-exit

With type info:
  - x declared as number
  - Aggressively compile double path
  - Possibly compile int path too (speculative)
  - Or in strict subset: compile only int path, no guard
```

---

## Other Directions

### Immutability Inference

Most objects in serious codebases are effectively immutable after construction:

```typescript
const user = { name: "Alice", age: 30, role: "admin" };
// used everywhere, never modified
```

Nobody declared `readonly`. Whole-program analysis could prove it's never mutated.

Implications:
- No write barriers (GC optimization)
- Can share across threads
- Can deduplicate (same values → same memory)
- Can allocate in read-only memory
- Enables more aggressive inlining

### Purity Inference

Which functions have no side effects?

```typescript
function distance(a: Point, b: Point): number {
    return Math.sqrt((a.x - b.x) ** 2 + (a.y - b.y) ** 2);
}
```

If `distance` is pure:
- Same args can deduplicate calls
- Results can be memoized
- Calls can be reordered or parallelized
- Dead calls can be eliminated
- Compile-time evaluation when args known

JITs can't do this well because purity depends on whole call graph.

### Effect Inference

Track what effects a function can have:

```
reads: { a.x, a.y, b.x, b.y }
writes: { }
allocates: { }
throws: { never }
calls: { Math.sqrt }
```

Like Rust's borrow checker but inferred. Two functions with non-overlapping effects can run in parallel.

### Lifetime Inference

Where does an object need to live?

```typescript
function compute(): number {
    const temp = { x: 1, y: 2 };  // only used locally
    return temp.x + temp.y;
}
```

We could infer:
- `temp` doesn't escape this function
- `temp` doesn't outlive this stack frame
- Stack allocate (or eliminate entirely)

More interesting:

```typescript
function processItems(items: Item[]): Result[] {
    return items.map(item => {
        const intermediate = transform(item);  // lives only within iteration
        return finalize(intermediate);
    });
}
```

Each `intermediate` lives for one iteration. Could:
- Reuse same memory slot across iterations
- Stack allocate single slot instead of N heap allocations

### Allocation Hoisting

```typescript
for (let i = 0; i < 1000000; i++) {
    const point = { x: data[i*2], y: data[i*2+1] };
    results.push(processPoint(point));
}
```

Even if `point` escapes into `processPoint`, maybe it doesn't escape after `processPoint` returns. Could:
- Allocate one Point-shaped slot before loop
- Rewrite values each iteration
- If `processPoint` stores reference, copy-on-store

### Shape Prediction vs Observation

JITs observe shapes at runtime. We could predict from code:

```typescript
function createResponse(status: number, data: any) {
    return { status, data, timestamp: Date.now() };
}
```

Without running, we know return shape: `{ status: number, data: any, timestamp: number }`. Property order, everything.

```typescript
const resp = createResponse(200, payload);
console.log(resp.status);  // Known offset, known type
```

JIT needs to run code, build IC entries, maybe deopt. We just know.

### Prototype Chain Flattening

JS has prototype inheritance. V8's optimized code walks prototype chains (or caches them with invalidation).

In closed world:

```typescript
class Animal {
    breathe() { /* ... */ }
}

class Dog extends Animal {
    bark() { /* ... */ }
}
```

We see all classes. `Dog` never extended. Flatten:

```
Dog instance layout:
  - shape pointer
  - own properties
  - breathe: function (inlined from Animal)
  - bark: function
```

No prototype chain. Method calls become direct offset lookups or inlined code.

### Iterator Protocol Elimination

```typescript
for (const item of items) {
    process(item);
}
```

JS semantics: call `items[Symbol.iterator]()`, repeatedly call `.next()`, check `.done`.

If we know `items` is Array and iteration protocol untampered:

```llvm
; Just a counted loop with indexed access
for i = 0 to length(items):
    process(items[i])
```

Deoptimization in JITs (with guards). Proven fact in closed-world.

### String Value Analysis

Strings usually treated as opaque. But:

```typescript
type Status = "pending" | "active" | "complete";

function getColor(status: Status): string {
    switch (status) {
        case "pending": return "yellow";
        case "active": return "green";
        case "complete": return "blue";
    }
}
```

We know:
- `status` is one of exactly 3 strings
- Compile-time constants
- Switch is exhaustive

Could:
- Intern strings (pointer comparison instead of string comparison)
- Compile to integer switch (0, 1, 2)
- Maybe eliminate runtime representation entirely

### Async/Await Optimization

Async functions compile to state machines. But often:

```typescript
async function fetchUser(id: string): Promise<User> {
    const cached = cache.get(id);
    if (cached) return cached;  // Synchronous return
    const user = await api.fetch(id);
    cache.set(id, user);
    return user;
}
```

Cache hit path is synchronous. JIT compiles whole thing as async machinery. Could:
- Detect fast path is sync
- Generate specialized sync version
- Only pay async overhead when actually awaiting

### Object Layout Optimization

If we see all accesses to a type:

```typescript
interface User { id: string; name: string; email: string; lastLogin: Date; }

// Access patterns:
user.id        // 1000x
user.name      // 1000x
user.lastLogin // 5x
user.email     // 2x
```

Lay out:

```
User layout:
  offset 0:  id        (hot)
  offset 8:  name      (hot)
  offset 16: lastLogin (cold)
  offset 24: email     (cold)
```

Hot fields in first cache line.

### Bounds Check Elimination

```typescript
function sumArray(arr: number[]): number {
    let sum = 0;
    for (let i = 0; i < arr.length; i++) {
        sum += arr[i];  // bounds check here
    }
    return sum;
}
```

Bounds check is redundant—`i < arr.length`. JITs do this but worry about:
- Array mutation during loop
- Concurrent modification
- Overflow in index computation

In closed-world, might prove `arr` isn't mutated, eliminate check with certainty.

### Call Graph Specialization

```typescript
function process(items: Item[], transformer: (item: Item) => Result): Result[] {
    return items.map(transformer);
}

// Only ever called with:
process(items, basicTransform);
process(items, advancedTransform);
```

We see all call sites. `transformer` is one of two functions. Could:
- Generate two specialized versions
- Direct call instead of indirect
- Inline both transformers entirely

---

## GC and LLVM

### The Problem

When compiling to native, GC needs to find all live object references at collection time:
- In CPU registers
- Spilled to stack slots
- In global variables
- Inside heap objects

For heap objects, you control layout. Hard part: stack and registers.

### LLVM Statepoints

LLVM provides precise GC infrastructure through statepoints:

**LLVM 18+ syntax:** statepoint callees must be written as `ptr elementtype(<fn sig>) @callee` in textual IR, and the
statepoint varargs must end with two *constant* `i32` counts: `num_transition_args` and `num_deopt_args` (use `0, 0`;
inline transition args are deprecated).

```llvm
declare ptr addrspace(1) @allocate(i64)

define ptr addrspace(1) @make_pair(ptr addrspace(1) %a, ptr addrspace(1) %b) gc "coreclr" {
entry:
    ; About to call allocate(), which might trigger GC.
    ; Tell LLVM %a and %b are live GC references at this safepoint.
    %safepoint = call token (i64, i32, ptr, i32, i32, ...)
        @llvm.experimental.gc.statepoint.p0(
            i64 0, i32 0,
            ptr elementtype(ptr addrspace(1) (i64)) @allocate,
            i32 1, i32 0,
            i64 16,          ; call argument
            i32 0, i32 0     ; num_transition_args, num_deopt_args
        ) [ "gc-live"(ptr addrspace(1) %a, ptr addrspace(1) %b) ]

    %pair = call ptr addrspace(1) @llvm.experimental.gc.result.p1(token %safepoint)

    ; If allocate() triggers a moving GC, %a/%b might have been relocated.
    ; Indices in gc.relocate refer to the order in the "gc-live" bundle above.
    %a.relocated = call coldcc ptr addrspace(1) @llvm.experimental.gc.relocate.p1(
        token %safepoint, i32 0, i32 0)
    %b.relocated = call coldcc ptr addrspace(1) @llvm.experimental.gc.relocate.p1(
        token %safepoint, i32 1, i32 1)

    store ptr addrspace(1) %a.relocated, ptr addrspace(1) %pair, align 8
    %pair.1 = getelementptr ptr, ptr addrspace(1) %pair, i64 1
    store ptr addrspace(1) %b.relocated, ptr addrspace(1) %pair.1, align 8
    ret ptr addrspace(1) %pair
}
```

Verify this snippet on LLVM 18 (save the IR above as `/tmp/statepoint.ll`):

```bash
llvm-as /tmp/statepoint.ll -o /dev/null  # prints nothing on success; verifier errors otherwise
```

LLVM generates native code + stack maps (metadata describing where GC refs are at each safepoint).

**Statepoint stackmap invariant (runtime requirement):**
LLVM StackMaps *can* legally encode `gc-live` values in registers (`Register`) or as computed
expressions (`Direct`). Our first runtime milestone uses **frame-pointer-only stack walking** and
does not reconstruct a full register context for every frame, so we require statepoint roots be
addressable stack slots (`Indirect [SP + off]`).

In practice, with our configured LLVM 18 codegen (notably `--fixup-allow-gcptr-in-csr=false` and/or
`--fixup-max-csr-statepoints=0`), we observe that on both **x86_64 SysV** and **aarch64 SysV**,
across `-O0/-O2` and with/without `-frame-pointer=all`, statepoint `gc-live` roots are emitted as
`Indirect [SP + off]` spill slots.

This is good news: a moving GC can update the spill slot in memory, and the code after the
statepoint reloads relocated values from those slots.

If LLVM ever starts emitting register roots for `gc-live`, our runtime must grow full
register-context capture + restore support; until then we should assert/fail loudly if a non-stack
GC root location kind is observed.

See also: `docs/stackmaps.md` (required codegen flags + verifier tests).

Repro (LLVM 18):
```bash
# out.ll contains `gc.statepoint` + `gc.relocate` after rewriting
opt-18 -passes=rewrite-statepoints-for-gc -S in.ll -o out.ll
llc-18 -O2 --fixup-allow-gcptr-in-csr=false --fixup-max-csr-statepoints=0 -filetype=obj out.ll -o out.o
llvm-readobj-18 --stackmap out.o

# Cross-check AArch64:
llc-18 -mtriple=aarch64-unknown-linux-gnu -O2 --fixup-allow-gcptr-in-csr=false --fixup-max-csr-statepoints=0 -filetype=obj out.ll -o out_aarch64.o
llvm-readobj-18 --stackmap out_aarch64.o
```

Runtime reads stack maps. When GC runs:

```c
void collect() {
    for (StackFrame* frame = current_frame; frame; frame = frame->parent) {
        StackMap* map = lookup_stackmap(frame->return_address);
        for (int i = 0; i < map->num_slots; i++) {
            GCRef* ref = (GCRef*)((char*)frame + map->slot_offsets[i]);
            *ref = relocate_if_needed(*ref);
        }
    }
}
```

### GC Strategies

Option A: Non-moving mark-sweep (simplest)

Objects never move, don't need `gc.relocate`:

```c
void mark_sweep() {
    for_each_root(root -> mark_recursive(*root));
    for_each_object(obj -> {
        if (!is_marked(obj)) free(obj);
        else clear_mark(obj);
    });
}
```

Pros: simple, no pointer updates. Cons: fragmentation, poor cache locality over time.

Option B: Copying/compacting

Objects move:
- Automatic compaction
- Bump-pointer allocation (fast)
- Better cache locality

```c
void* allocate(size_t size) {
    if (from_space.alloc_ptr + size > from_space.end) {
        collect();  // copy live objects to to_space, swap
    }
    void* result = from_space.alloc_ptr;
    from_space.alloc_ptr += size;
    return result;
}
```

Requires `gc.relocate`—after copy, pointers need updating.

Option C: Generational (production)

Most objects die young:

```
┌─────────────────┐
│    Nursery      │  <- Fast bump allocation, collected frequently
│   (Young Gen)   │     Survivors promoted to old gen
├─────────────────┤
│   Old Gen       │  <- Collected less frequently
└─────────────────┘
```

Need write barriers to track old→young pointers:

```c
void write_field(Object* obj, int field_idx, Object* value) {
    obj->fields[field_idx] = value;
    if (is_old(obj) && is_young(value)) {
        remembered_set_add(obj);
    }
}
```

### V8's Orinoco GC

For reference:
- Young gen: semi-space copying collector (Scavenger)
- Old gen: incremental mark-sweep with compaction
- Write barriers: inline code at every pointer store
- Incremental marking: interleaved with JS execution
- Concurrent sweeping: background threads

This all works with AOT code. V8's JIT-compiled code includes write barriers, safepoint polls, stack maps.

---

## LLVM: Ours vs Theirs

### LLVM Does Well

Scalar optimization (given clean IR with known types):
- DCE, CSE
- Constant propagation and folding
- Strength reduction (`x * 2` → `x << 1`)
- Algebraic simplification

Loop optimization (given simple loop structures):
- Unrolling
- LICM
- Induction variable simplification
- Vectorization (auto-SIMD)
- Loop interchange/fusion

Register allocation and instruction selection: world-class, don't touch it.

Cross-function optimization (with LTO):
- Inlining
- Dead function elimination
- Cross-module constant propagation

### LLVM Doesn't Know

JS semantics:
- What does `+` mean? (number add, string concat, valueOf...)
- What's a shape? (LLVM sees opaque pointers)
- What's a prototype chain? (LLVM sees memory)

Your type system:
- TS types erased before LLVM sees anything
- LLVM has `i64`, `double`, `ptr`—not `User` or `Array<Point>`

GC roots:
- LLVM can track them (statepoints) but we define what is a root

High-level patterns:
- "Property access" vs "load instruction"
- "Method call" vs "indirect call"

### Division of Labor

We do semantic analysis:
- Type inference and narrowing
- Shape analysis
- Escape analysis
- Purity analysis
- Devirtualization decisions

Then lower to LLVM IR exposing the optimization:

```typescript
// Original
point.x + point.y

// After analysis: we know Point shape, x at offset 8, y at offset 16

// LLVM IR:
%p.x = load double, ptr getelementptr (%Point, %point, 0, 1)
%p.y = load double, ptr getelementptr (%Point, %point, 0, 2)
%sum = fadd double %p.x, %p.y
```

LLVM sees: two loads from known offsets, a double addition. LLVM optimizes perfectly. We did the hard part (proving shape, proving types). LLVM does the easy part (good machine code).

### float→int Specifically

LLVM has `fptosi`/`fptoui` (float to int) and `sitofp`/`uitofp` (int to float).

But LLVM won't decide to use integers instead of floats. That's our analysis:

```
Our analysis: "factorial always called with integers, arithmetic stays in range"

We generate: i64 operations

If uncertain: generate f64 operations with fadd, fmul, etc.
```

LLVM optimizes either representation well. We choose which.

### We Build

1. TypeScript parser / type extractor
2. Our IR (typed, before lowering to LLVM)
3. Whole-program analysis passes (the novel stuff)
4. GC runtime (allocator, collector, write barriers)
5. Object model (shapes, property storage)
6. Standard library (optimized builtins)
7. Lowering to LLVM IR

### We Leverage From LLVM

1. Backend optimization
2. Code generation (multiple architectures)
3. LTO
4. GC statepoints
5. Sanitizers / debug info

### In Between

Inlining decisions: we have whole-program knowledge, know what should be inlined. But LLVM has good heuristics. Probably:
- Always-inline small functions (our decision)
- Provide `inlinehint` attributes (our guidance)
- Let LLVM make final call

Alias analysis: we might know more than LLVM (shapes, purity). Can annotate with `noalias`, `readonly`, etc. Or write custom LLVM pass that understands our object model.

---

## Speedup Sources

### Type Guard Elimination

JIT code has guards:

```javascript
// JIT-compiled:
if (typeof a !== 'number') goto deopt;
if (typeof b !== 'number') goto deopt;
result = a + b;  // finally the work
```

With proven types:

```llvm
; No guards. Known f64.
%result = fadd double %a, %b
```

Not just removing instructions—enabling further optimizations. Guards are optimization barriers:
- Prevent LICM (guard might fail)
- Prevent vectorization (can't SIMD through conditional deopt)
- Prevent instruction scheduling (dependency on guard)

### IC Elimination

Property access in JITs goes through IC machinery:

```
1. Load object's shape pointer
2. Compare to cached shape
3. If match, load from cached offset
4. If miss, slow path
```

Even fast path: load, compare, branch, load. Four operations minimum.

With proven shapes:

```llvm
; Known shape. Just load.
%value = load double, ptr getelementptr(%Point, %obj, 0, 1)
```

One operation. Now candidate for LLVM's load/store optimizations.

### Unboxing

JS values typically boxed or tagged:

```
SMI (V8):      [31-bit int][1-bit tag]
HeapNumber:    ptr -> { map, float64 value }
```

Even SMI requires tag manipulation. HeapNumbers require pointer dereference.

When you prove value is always `number` and used locally:

```llvm
; Just a register. No boxing.
%x = double 3.14
%y = fadd double %x, %x
```

### Devirtualization

Method calls in JIT (even optimized):

```
1. Load object shape
2. Check shape / lookup method
3. Indirect call through function pointer
```

Indirect calls block branch prediction, prevent inlining.

With proven sealed class:

```llvm
; Direct call. Can be inlined.
call double @Point_magnitude(%Point* %p)
```

With inlining:

```llvm
; No call. Just math.
%x = load double, ...
%y = load double, ...
; sqrt(x*x + y*y)
```

### Allocation Elimination

JITs do escape analysis but conservatively. Whole-program can be aggressive:

```typescript
function distance(x1: number, y1: number, x2: number, y2: number): number {
    const p1 = { x: x1, y: y1 };  // JIT: might escape, allocate
    const p2 = { x: x2, y: y2 };  // JIT: might escape, allocate
    return Math.sqrt((p1.x-p2.x)**2 + (p1.y-p2.y)**2);
}
```

JIT might not eliminate (depends on inlining budget, heuristics).

Whole-program proves they don't escape:

```llvm
define double @distance(double %x1, double %y1, double %x2, double %y2) {
    ; No allocations. Pure register math.
    %dx = fsub double %x1, %x2
    %dy = fsub double %y1, %y2
    %dx2 = fmul double %dx, %dx
    %dy2 = fmul double %dy, %dy
    %sum = fadd double %dx2, %dy2
    %result = call double @llvm.sqrt.f64(double %sum)
    ret double %result
}
```

### f64 Alone

Even if everything is f64, native still faster:
1. No tag checks (even V8's optimized code checks SMI vs HeapNumber)
2. No boxing (function calls don't wrap/unwrap)
3. Better register allocation (no hidden constraints from IC/deopt)
4. SIMD (vectorization straightforward with known f64 arrays)
5. Instruction scheduling (no barriers from guards)

Tight f64 loop compiled by LLVM beats V8's TurboFan. Not 10x, but 1.3-2x realistic for compute-heavy code.

---

## Graceful Degradation

Codebase will have parts that resist analysis.

### Tier A: Fully Proven

Types known, shapes known, no dynamism. Full native optimization.

```typescript
class Vector3 {
    constructor(public x: number, public y: number, public z: number) {}
    dot(other: Vector3): number {
        return this.x * other.x + this.y * other.y + this.z * other.z;
    }
}
```

Compiles to C-quality code.

### Tier B: Mostly Proven

Types known, some uncertainty at boundaries:

```typescript
function processData(input: unknown): Result {
    const data = validateAndParse(input);  // Guard here
    // rest is fully typed and optimized
}
```

One guard at entry, then fast code.

### Tier C: Dynamic But Contained

Some code uses dynamic features, but isolated:

```typescript
const config = JSON.parse(fs.readFileSync('config.json'));

function runApp(settings: AppSettings) { /* ... */ }
runApp(validateConfig(config));  // Guard at boundary
```

`JSON.parse` uses slower runtime. Typed section compiles native.

### Tier D: Actually Dynamic

```typescript
function dynamicAccess(obj: any, key: string) {
    return obj[key];  // Can't optimize
}
```

Fall back to runtime dictionary lookup. Rare in typed codebases.

In serious TypeScript codebases, most code is Tier A or B. Dynamic parts isolated at edges (config loading, API parsing).

---

## Pipeline

```
┌─────────────────────────────────────────────────────────────────┐
│                     TypeScript Codebase                         │
└────────────────────────────┬────────────────────────────────────┘
                             ↓
┌─────────────────────────────────────────────────────────────────┐
│                    1. Parse & Type Check                        │
│         (tsc or custom parser, preserve type annotations)       │
└────────────────────────────┬────────────────────────────────────┘
                             ↓
┌─────────────────────────────────────────────────────────────────┐
│                  2. Build Whole-Program IR                      │
│   - Call graph                                                  │
│   - Type flow graph                                             │
│   - Shape graph                                                 │
│   - Escape graph                                                │
└────────────────────────────┬────────────────────────────────────┘
                             ↓
┌─────────────────────────────────────────────────────────────────┐
│                  3. Whole-Program Analysis                      │
│   - Seal classes (not extended anywhere)                        │
│   - Finalize shapes (not modified anywhere)                     │
│   - Narrow types (actual types at each site)                    │
│   - Detect integer operations                                   │
│   - Compute escape sets                                         │
│   - Find devirtualization opportunities                         │
└────────────────────────────┬────────────────────────────────────┘
                             ↓
┌─────────────────────────────────────────────────────────────────┐
│                  4. Specialize & Optimize                       │
│   - Generate monomorphic variants                               │
│   - Inline based on whole-program info                          │
│   - Scalar replacement for non-escaping allocations             │
│   - Direct calls for sealed classes                             │
└────────────────────────────┬────────────────────────────────────┘
                             ↓
┌─────────────────────────────────────────────────────────────────┐
│                  5. LLVM IR Generation                          │
│   - Typed IR with known layouts                                 │
│   - GC integration (statepoints)                                │
│   - No speculation guards for proven facts                      │
└────────────────────────────┬────────────────────────────────────┘
                             ↓
┌─────────────────────────────────────────────────────────────────┐
│                  6. LLVM Optimization & Codegen                 │
│   - Standard LLVM passes                                        │
│   - Native binary output                                        │
└─────────────────────────────────────────────────────────────────┘
```

---

## Analysis Algorithms

### Class Sealing

```
INPUT: All class declarations, all expressions
OUTPUT: Set of sealed classes (never extended)

1. Build inheritance map: Map<Class, Set<Subclass>>
2. For each 'extends' clause, add edge
3. For each 'class X extends Y', mark Y as extended
4. Sealed classes = all classes with no subclasses
```

O(n) where n is number of class declarations.

### Shape Finalization

```
INPUT: All object creation sites, all property assignments
OUTPUT: For each creation site, final shape (or "dynamic")

1. Build creation site → shape mapping
   - Object literals: shape from literal
   - new Class(): shape from constructor
   
2. For each property assignment (obj.prop = x):
   - If obj's creation site is known:
     - If prop not in original shape: mark site as "dynamic"
     
3. Final shapes = creation sites not marked dynamic
```

Requires points-to analysis to connect assignments to creation sites.

### Type Flow (0-CFA style)

```
INPUT: Typed AST, call graph
OUTPUT: For each expression, set of concrete types that flow there

1. Initialize: each expression has declared type
2. For each call site f(x):
   - Add type(x) to parameter type of f
3. For each return site return y:
   - Add type(y) to return type of enclosing function
4. Iterate until fixed point
5. Intersect with declared types (declared types are upper bound)
```

Gives more precise types than declared. If `x: number | string` but only `number` flows there, you know it.

### Integer Detection

```
INPUT: Type flow results, operation sites
OUTPUT: Set of values that are always integers

1. For each arithmetic operation (+, -, *, /):
   - If both operands are known integers
   - And operation preserves integer (not /)
   - And no float literals involved
   - Then result is integer
   
2. For each number literal:
   - If written as integer (no decimal)
   - Mark as integer
   
3. For each Math.floor, Math.trunc, |0, etc:
   - Result is integer
   
4. Propagate through assignments
```

Generates integer code without programmer annotation.

---

## Open Questions

### JS Semantics Fidelity

Spectrum from "full JavaScript" to "JavaScript-like":

```javascript
[] + []  // ""
[] + {} // "[object Object]"
{} + [] // 0
```

Not easy to optimize. Implement correctly? Or say "don't write this"?

What about:

```javascript
const x = 1 / 0;    // Infinity
const y = 0 / 0;    // NaN
const z = -0;       // Negative zero
```

IEEE 754 is exact. Match exactly? Constrains optimization (can't assume `x / x == 1`).

What about sparse arrays:

```javascript
const arr = [1, 2, 3];
arr[10] = 4;  // [1, 2, 3, empty × 7, 4]
```

Slow everywhere but valid JS.

### Compile Time

Whole-program analysis is expensive:
- Type flow: O(n²) worst case
- Points-to: O(n³) for full precision
- Shape analysis: varies

30-second compiles? 5-minute compiles? Affects:
- Dev experience
- CI/CD
- Incremental compilation needs

### Deployment Model

Option A: Native binary
```
myapp.ts → compiler → myapp (ELF/Mach-O)
```

Option B: WASM
```
myapp.ts → compiler → myapp.wasm
```
Runs anywhere WASM runs.

Option C: Optimized JS
```
myapp.ts → compiler → myapp.js (optimized, still JS)
```
Emit JS structured to hit JIT fast paths.

Option D: Hybrid
```
myapp.ts → compiler → myapp.so + myapp.js
```
Hot code native, cold code interpreted. FFI between.

### Competition

Current options for fast TypeScript:
1. Write hot paths in Rust/C++, FFI from JS
2. WebAssembly (AssemblyScript, Rust→WASM)
3. Accept JIT performance (it's pretty good)
4. Bun/Deno (better runtime, same model)

Pitch:
- "Write normal TypeScript"
- "Get native performance"
- "No language learning curve"
- "Your existing codebase, faster"

### npm Dependencies

Options:
1. Include in analysis if TypeScript
2. Trust `.d.ts` types as boundaries (insert guards)
3. Ship optimized versions of common libraries

### Type Assertions

```typescript
const x = something as Point;
```

Options:
1. Trust (unsafe, fast)
2. Runtime check (verify shape)
3. Reject (require type guard)

Lean toward (2) with (1) as opt-in.

### Structural Typing

```typescript
interface HasX { x: number }
function readX(obj: HasX): number { return obj.x; }

readX({ x: 1, y: 2 });  // Point-ish
readX({ x: 1, z: 3 });  // Different shape
```

Approaches:
1. Multiple shapes: `readX` works with multiple layouts, dispatch on shape
2. Uniform offset: all "HasX-compatible" shapes put `x` at same offset

(2) is how Go interfaces work.

---

## JIT vs AOT Comparison

| Aspect | JIT (V8 TurboFan) | Whole-Program AOT |
|--------|-------------------|-------------------|
| Class sealing | Speculative (can be invalidated) | Proven (no guards) |
| Shape stability | Observed (IC, can miss) | Proven (direct offset) |
| Type narrowing | Per-function, profile-based | Whole-program, static |
| Inlining budget | Limited (compilation time) | Aggressive (compile once) |
| Escape analysis | Intra-procedural mostly | Inter-procedural |
| Startup time | Slow (interpretation, baseline, optimize) | Fast (native from start) |
| Steady-state | Excellent for hot code | Excellent always |
| Memory | High (multiple tiers, feedback) | Lower (single compiled form) |

### Where AOT Wins

- Predictability (games, audio, real-time—JIT pauses and deopts are unacceptable)
- Cold code (no warmup)
- Complex but provable types (JIT can't infer what you know)
- Deopt-guard-dominated code (tight loops with many type checks)
- Memory-constrained (no metadata overhead)

### Where AOT Doesn't Help

- Already monomorphic and hot code (JIT optimizes fully)
- Truly dynamic patterns (both are slow)
- Short-running scripts (compilation overhead dominates)

---

## References

### Papers

- "Gradual Typing for Functional Languages" (Siek & Taha) — foundational
- "Is Sound Gradual Typing Dead?" (Takikawa et al.) — boundary overhead
- "Concrete Type Inference" (Agesen) — Self VM work
- "An Efficient Implementation of SELF" (Chambers, Ungar) — hidden classes origin
- "Truffle: A Self-Optimizing Runtime System" — GraalVM approach
- "Deoptimization in V8" (Google blog posts) — practical deopt

### Codebases

V8 (chromium.googlesource.com/v8/v8):
- `src/compiler/` — TurboFan
- `src/ic/` — inline caches
- `src/objects/map.h` — hidden class (Shape)

SpiderMonkey:
- `js/src/jit/` — JIT compilers
- `js/src/vm/Shape.h` — shapes

Porffor: experimental TS AOT

### LLVM Reference

Value representation:

```llvm
; NaN-boxed (64-bit)
; If valid double (not NaN), it's a number
; If NaN, payload encodes pointer or tag
%JSValue = type i64

; Tagged union:
%JSValue = type { i8, [7 x i8] }  ; tag + payload
```

Object layout:

```llvm
%JSObject = type {
    %Shape*,           ; hidden class
    %JSValue*,         ; properties array
    i32,               ; flags (extensible, frozen, etc.)
}

%Shape = type {
    %Shape*,           ; transition parent
    i32,               ; property count
    %PropertyDescriptor*,
}
```

---

## Next Steps (When Ready)

1. Define scope: research prototype? production tool? new runtime?
2. Pick initial target: WASM (easier) or native (more complex)?
3. Prototype analysis: type flow + shape analysis on small codebase
4. Measure against JITs: quantify actual speedups
5. GC strategy: decide on collector architecture early
