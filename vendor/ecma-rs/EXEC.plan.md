# EXEC.plan.md — Implementation Plan

Concrete end-to-end plan. No holding back.

> **See also:** `EXEC.md` for background discussion, research notes, and design exploration that led to this plan.

---

## System Requirements (Ubuntu x64)

Install once per machine before running agents:

```bash
# Core build tools
sudo apt-get update
sudo apt-get install -y \
  build-essential \
  pkg-config \
  libssl-dev \
  util-linux \
  git \
  curl

# Rust toolchain (if not present)
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
source "$HOME/.cargo/env"
rustup default stable

# LLVM 18 (for native codegen backend)
# Using LLVM's official apt repo for latest stable
wget -qO- https://apt.llvm.org/llvm-snapshot.gpg.key | sudo tee /etc/apt/trusted.gpg.d/apt.llvm.org.asc
echo "deb http://apt.llvm.org/$(lsb_release -cs)/ llvm-toolchain-$(lsb_release -cs)-18 main" | \
  sudo tee /etc/apt/sources.list.d/llvm-18.list
sudo apt-get update
sudo apt-get install -y \
  llvm-18 \
  llvm-18-dev \
  clang-18 \
  lld-18

# Set LLVM paths (add to ~/.bashrc or agent env)
export LLVM_SYS_180_PREFIX=/usr/lib/llvm-18
export PATH="/usr/lib/llvm-18/bin:$PATH"
```

### What's Required

| Package | Why |
|---------|-----|
| `build-essential` | gcc, make, etc. for linking |
| `util-linux` | `flock`, `prlimit` for resource limiting |
| `llvm-18`, `llvm-18-dev` | LLVM backend for native codegen |
| `clang-18`, `lld-18` | Compiler/linker for native code |
| Rust stable | Compilation |

### Verification

```bash
# Run the system check script
bash vendor/ecma-rs/scripts/check_system.sh

# Or manually:
rustc --version
llvm-config-18 --version
flock --version
prlimit --version
```

---

## Agent Resource Guidelines

**Context:** Hundreds of concurrent coding agents on one system (192 vCPU, 1.5TB RAM, 110TB disk).

**Critical constraint:** RAM. Too many concurrent memory-heavy processes will OOM-kill everything.

**Not a constraint:** CPU and disk I/O. Scheduler handles contention fine. Don't be overly conservative.

**Vendored checkout note:** In this repository, ecma-rs lives under `vendor/ecma-rs/` as a nested
workspace. The commands below are written to run from the **top-level repo root**. If you've
already `cd vendor/ecma-rs`, drop the `vendor/ecma-rs/` prefix from paths (scripts and `target/`).

### Rules

**1. Always use the wrapper scripts:**
```bash
# CORRECT:
bash vendor/ecma-rs/scripts/cargo_agent.sh build --release -p native-js
bash vendor/ecma-rs/scripts/cargo_agent.sh test -p effect-js --lib

# WRONG (will spawn uncontrolled parallelism):
cargo build
cargo test
```

The wrapper (`vendor/ecma-rs/scripts/cargo_agent.sh`) `cd`s into `vendor/ecma-rs/` and delegates to
the top-level `scripts/cargo_agent.sh` wrapper. It enforces:
- Slot-based concurrency limiting (prevents cargo stampedes)
- Per-command RAM cap via `RLIMIT_AS` (default 64GB)
- Reasonable test thread counts

**2. Scope your cargo commands:**
```bash
# CORRECT (scoped to specific crate):
bash vendor/ecma-rs/scripts/cargo_agent.sh test -p native-js --lib
bash vendor/ecma-rs/scripts/cargo_agent.sh build -p effect-js

# WRONG (compiles entire workspace):
bash vendor/ecma-rs/scripts/cargo_agent.sh build --all
bash vendor/ecma-rs/scripts/cargo_agent.sh test
```

**3. LLVM operations need extra RAM:**

LLVM compilation is memory-hungry. Use the LLVM wrapper for native codegen:
```bash
# Preferred: use the LLVM wrapper (sets 96GB limit automatically):
bash vendor/ecma-rs/scripts/cargo_llvm.sh build -p native-js
bash vendor/ecma-rs/scripts/cargo_llvm.sh test -p native-js --lib

# Or set manually:
FASTR_CARGO_LIMIT_AS=96G bash vendor/ecma-rs/scripts/cargo_agent.sh test -p native-js --lib

# For full release builds with LTO (very hungry):
FASTR_CARGO_LIMIT_AS=128G bash vendor/ecma-rs/scripts/cargo_agent.sh build --release -p native-js
```

**4. Don't artificially limit parallelism:**
```bash
# WRONG (too conservative - wastes resources):
FASTR_CARGO_JOBS=1 bash vendor/ecma-rs/scripts/cargo_agent.sh build ...

# RIGHT (let the wrapper decide based on available slots):
bash vendor/ecma-rs/scripts/cargo_agent.sh build ...

# RIGHT (if you need to limit for a specific reason, document why):
# Reduce parallelism because this test spawns subprocesses
FASTR_CARGO_JOBS=8 bash vendor/ecma-rs/scripts/cargo_agent.sh test ...
```

**5. Long-running processes need timeouts:**
```bash
# Running compiled binaries - always with limits:
bash vendor/ecma-rs/scripts/run_limited.sh --as 32G --cpu 300 -- ./vendor/ecma-rs/target/release/my_binary

# Don't run indefinitely:
timeout 600 bash vendor/ecma-rs/scripts/run_limited.sh --as 32G -- ./vendor/ecma-rs/target/release/my_binary
```

**6. Clean up disk when over budget:**
```bash
# Before long loops, check `vendor/ecma-rs/target/` size:
TARGET_MAX_GB="${TARGET_MAX_GB:-400}"
if [[ -d vendor/ecma-rs/target ]]; then
  size_gb=$(du -sg vendor/ecma-rs/target 2>/dev/null | cut -f1 || echo 0)
  if [[ "${size_gb}" -ge "${TARGET_MAX_GB}" ]]; then
    echo "vendor/ecma-rs/target at ${size_gb}GB, cleaning..." >&2
    bash vendor/ecma-rs/scripts/cargo_agent.sh clean
  fi
fi
```

### Resource Estimates

| Operation | RAM (per process) | Notes |
|-----------|-------------------|-------|
| `cargo check -p crate` | 2-8 GB | Depends on crate size |
| `cargo build -p crate` | 4-16 GB | Debug build |
| `cargo build --release -p crate` | 8-32 GB | Release + optimizations |
| `cargo build --release` (LTO) | 32-96 GB | Full workspace LTO |
| `cargo test -p crate` | 4-16 GB | Depends on test count |
| LLVM codegen (our native-js) | 16-64 GB | Per compilation unit |
| Running compiled binary | 1-32 GB | Depends on workload |

### What NOT to Worry About

- **CPU contention**: Scheduler handles it. If 200 agents all want CPU, they get time-sliced.
- **Disk I/O contention**: NVMe handles parallel I/O well. Don't serialize disk operations.
- **Network**: Not relevant for compilation.
- **Concurrent git operations**: Each agent has own repo copy. No conflicts.

### Quick Reference

```bash
# Standard build/test (most operations):
bash vendor/ecma-rs/scripts/cargo_agent.sh build -p <crate>
bash vendor/ecma-rs/scripts/cargo_agent.sh test -p <crate> --lib

# LLVM-heavy operations (native-js, runtime-native):
bash vendor/ecma-rs/scripts/cargo_llvm.sh build -p native-js
bash vendor/ecma-rs/scripts/cargo_llvm.sh test -p native-js --lib

# Or with explicit limit:
FASTR_CARGO_LIMIT_AS=96G bash vendor/ecma-rs/scripts/cargo_agent.sh <command>

# Running binaries:
bash vendor/ecma-rs/scripts/run_limited.sh --as 32G --cpu 300 -- ./vendor/ecma-rs/target/release/binary

# Check if target/ needs cleaning:
du -sh vendor/ecma-rs/target/
```

---

## Key Decisions

### Our TypeScript Dialect ("Strict Mode")

We compile a **strict subset** of TypeScript with hard enforcement:

```
REJECTED (compile error, not warning):
  - `any` type (explicit or inferred)
  - Type assertions that lie (`x as T` where x is not T)
  - Non-null assertions on nullable values (`x!` where x might be null)
  - `eval()`, `new Function()`
  - `with` statement
  - `arguments` object
  - Prototype mutation after construction
  - Computed property access with non-constant keys (in strict paths)
  - `Proxy` (or heavily restricted)

RESTRICTED:
  - Union types: allowed, but generate tag-checked code
  - `unknown`: allowed, requires narrowing before use
  - Dynamic property access: falls back to slow path with warning

ALLOWED (full support):
  - All primitive types
  - Interfaces, type aliases, generics
  - Classes (single inheritance, nominal for our purposes)
  - Tagged/discriminated unions (first-class optimization)
  - Literal types, const assertions
  - Tuples
  - readonly modifiers
  - Async/await, Promises
  - Modules, imports/exports
```

**Precedent:** This is similar to:
- **AssemblyScript**: Strict TypeScript subset for WASM. No `any`, no union types in some contexts.
- **Static TypeScript (Microsoft)**: Subset for embedded systems, compiles to ARM Thumb. Omits `eval`, prototype-based inheritance.

We're stricter than regular `tsc --strict`. We reject code that tsc would accept.

### What LLVM Handles

We generate LLVM IR. LLVM handles everything below that:

```
WE DO (in our compiler):
  - Parsing (parse-js)
  - Type checking (typecheck-ts)
  - HIR → MIR lowering
  - Effect analysis, escape analysis, ownership inference
  - High-level optimizations (inlining, devirtualization, fusion, etc.)
  - LLVM IR generation
  - GC safepoint insertion (using LLVM statepoints)

LLVM DOES (we don't touch):
  - Instruction selection (IR → machine instructions)
  - Register allocation (graph coloring)
  - Instruction scheduling
  - Low-level optimizations (peephole, etc.)
  - x86/ARM/etc. code emission
  - Object file generation
  
RUNTIME DOES (linked with output):
  - GC implementation (marking, sweeping, compacting)
  - Memory allocation
  - Stack map interpretation (for GC)
  - Parallel task scheduler
  - Async event loop
```

**LLVM Statepoints for GC:** Confirmed working. Used by:
- GraalVM Native Image
- JLLVM (Java on LLVM)
- Supports x86_64 and AArch64

We use `@llvm.experimental.gc.statepoint` intrinsics. LLVM generates stack maps automatically. Our runtime reads stack maps at GC time.

---

## Research Findings

### Prior Art: TypeScript-to-Native

| Project | Approach | Notes |
|---------|----------|-------|
| **AssemblyScript** | TS subset → WASM | No `any`, explicit types, own compiler |
| **Static TypeScript** | TS subset → ARM Thumb | Microsoft, for embedded/education |
| **Hermes** | JS → bytecode (AOT) | Meta, for React Native, not native code |
| **GraalJS** | JS via Truffle → native | JIT with partial evaluation |
| **TypeScript 7 (Corsa)** | TS compiler in Go | Faster compiler, still emits JS |

We're closest to AssemblyScript's strictness + LLVM backend (not WASM).

### Why We Don't Need Deoptimization

V8/TurboFan approach:
```
1. Run interpreted
2. Profile types at runtime
3. Speculate: "x is always a number"
4. Generate optimized code with guards
5. If guard fails → deoptimize → back to interpreter
```

Our approach:
```
1. Types are known at compile time (TypeScript)
2. No speculation needed
3. Generate optimized code directly
4. No guards, no deoptimization
5. If types could vary → generate dispatch (known variants)
```

This is fundamentally different. V8 hopes types are stable. We know types are stable.

### What V8 Learns at Runtime, We Know at Compile Time

| V8 (runtime discovery) | Us (compile-time knowledge) |
|------------------------|----------------------------|
| Hidden class transitions | Static shapes from types |
| Inline cache hits/misses | Direct offset access |
| Hot functions | Analyze all functions |
| Array element kinds | TypeScript tells us |
| Type feedback | TypeScript types |

---

## Vision

Write normal TypeScript. Get native performance.

No annotations. No special syntax. No "typed regions." No breaking the language.

```typescript
async function main() {
  const users = await db.query('SELECT * FROM users');
  const enriched = users.map(enrichUser);
  const report = generateReport(enriched);
  await email.send(report);
}
```

Compiler infers:
- **Ownership**: `users` is consumed by `map`, never used again → no GC needed, transfer ownership
- **Effects**: `enrichUser` is pure (reads user, returns enriched) → can parallelize, can stream
- **Streaming**: `map` feeds into `generateReport` → don't materialize intermediate array, stream
- **Parallelization**: `enrichUser` has no shared state → parallelize if array is large enough

Output:
- Native code for CPU-bound parts
- Streaming pipeline (start enriching before all users fetched)
- Automatic parallelization of the map
- Zero intermediate allocations (fused pipeline)
- GC only for genuinely long-lived objects

The developer writes normal code. The compiler does the work.

---

## The Three Inferences

### 1. Effect Inference

Every expression has effects. Infer them.

```typescript
function enrichUser(user: User): EnrichedUser {
  return {
    ...user,
    fullName: `${user.firstName} ${user.lastName}`,
    ageGroup: getAgeGroup(user.age)
  };
}

// Compiler infers:
// - Reads: user (argument)
// - Writes: nothing
// - Allocates: yes (new object)
// - I/O: no
// - Throws: no
// - PURE: yes (given same input, same output)
```

No annotation. The compiler reads the code. It knows `getAgeGroup` is pure (it analyzed that too). It knows spreading an object and string concatenation are pure. It concludes: `enrichUser` is pure.

**What this enables:**
- Pure functions can be parallelized
- Pure functions can be memoized
- Pure functions can be reordered
- Pure functions can be evaluated at compile time (if inputs are known)

```typescript
users.map(enrichUser)  // enrichUser is pure → parallelize if users is large
```

### 2. Ownership Inference

Track who "owns" each value. Like Rust, but inferred.

```typescript
function processData(items: Item[]): Result {
  const transformed = items.map(transform);  // items still owned here
  const filtered = transformed.filter(isValid);  // transformed consumed
  return aggregate(filtered);  // filtered consumed
}
// items: borrowed (caller still owns)
// transformed: created here, consumed by filter, never used again
// filtered: created here, consumed by aggregate, never used again
// Result: returned to caller (ownership transferred)
```

**Inference rules:**
- Value assigned and never used again → ownership transferred (no GC)
- Value used once then discarded → can be consumed (no copy)
- Value used multiple times → shared reference (may need GC)
- Value escapes function → returned or stored (ownership transferred out)

**What this enables:**
- Most intermediate values never touch GC
- Arrays can be reused instead of allocated (map in place when source is consumed)
- Known lifetimes → stack allocation

```typescript
const transformed = items.map(transform);
const filtered = transformed.filter(isValid);
// transformed is never used after filter
// → filter can mutate transformed in place
// → no intermediate array allocation
```

### 3. Streaming Inference

Detect pipelines. Stream them.

```typescript
const result = users
  .map(enrichUser)
  .filter(isActive)
  .map(formatForReport)
  .reduce(combineRows, initialReport);
```

Traditional: 4 passes, 3 intermediate arrays.

Inferred:
- This is a pipeline (chain of array operations)
- Each step consumes previous, produces next
- Final step is reduce (single output)
- **Stream**: process each user through entire pipeline before next user

```
// Conceptual execution:
for each user in users:
  enriched = enrichUser(user)
  if isActive(enriched):
    formatted = formatForReport(enriched)
    result = combineRows(result, formatted)
```

One pass. Zero intermediate arrays. Cache-friendly.

**Detection:**
- Chain of `.map()`, `.filter()`, `.reduce()`, `.find()`, etc.
- No intermediate result used elsewhere
- Final operation is terminal (reduce, find, forEach, etc.) OR result is iterated once

---

## Verification from Assertions

You don't need special `@verify` annotations. Use normal assertions.

```typescript
function sqrt(x: number): number {
  assert(x >= 0, 'sqrt requires non-negative input');
  return Math.sqrt(x);
}

function distance(a: Point, b: Point): number {
  const dx = b.x - a.x;
  const dy = b.y - a.y;
  const squared = dx * dx + dy * dy;
  // Compiler knows: squared >= 0 (sum of squares)
  // Compiler knows: sqrt's precondition is satisfied
  // → No runtime check needed for sqrt call
  return sqrt(squared);
}
```

**How it works:**
1. Assertions are preconditions/postconditions
2. Compiler tries to prove them from context
3. If proven → no runtime check (it's guaranteed)
4. If not proven → keep runtime check (safety)
5. If proven false → compile error (dead code or bug)

```typescript
function divide(a: number, b: number): number {
  assert(b !== 0, 'division by zero');
  return a / b;
}

function average(items: number[]): number {
  assert(items.length > 0, 'cannot average empty array');
  return items.reduce((a, b) => a + b, 0) / items.length;
  // Compiler knows: items.length > 0 (from assertion)
  // Compiler knows: division is safe
}

function processNonEmpty(items: number[]): number {
  if (items.length === 0) return 0;
  // Compiler knows: items.length > 0 here
  return average(items);  // precondition satisfied, no check
}
```

**Conditional narrowing feeds verification:**
```typescript
function process(x: number | null): number {
  if (x === null) return 0;
  // x is narrowed to number
  // Any assertions about x being non-null are proven
  return x * 2;
}
```

**Gradual verification:**
- Start: all assertions are runtime checks
- As compiler gets smarter: more assertions proven, fewer runtime checks
- You don't change your code, it just gets faster
- If you add more assertions, compiler has more to work with

---

## Full Example: What the Compiler Sees

```typescript
// What you write (normal TypeScript):

interface User {
  id: number;
  name: string;
  email: string;
  lastLogin: Date;
}

interface Report {
  activeUsers: number;
  totalLogins: number;
  userList: string[];
}

async function generateDailyReport(): Promise<Report> {
  const users = await db.query<User>('SELECT * FROM users');
  
  const activeUsers = users.filter(u => 
    daysSince(u.lastLogin) < 30
  );
  
  const report: Report = {
    activeUsers: activeUsers.length,
    totalLogins: activeUsers.reduce((sum, u) => sum + u.loginCount, 0),
    userList: activeUsers.map(u => u.name)
  };
  
  return report;
}
```

**Compiler analysis:**

```
Function: generateDailyReport
  Effects: async, I/O (db.query), allocates
  
  db.query<User>('SELECT * FROM users')
    Returns: User[] (streaming possible)
    Effects: I/O, async
    
  users.filter(u => daysSince(u.lastLogin) < 30)
    Callback analysis:
      - Reads: u.lastLogin
      - Calls: daysSince (pure, no I/O)
      - Returns: boolean
      - PURE: yes
    Result: User[] (subset)
    users is not used after this → can consume
    
  activeUsers is used 3 times:
    - .length
    - .reduce(...)
    - .map(...)
  These can be fused into single pass.
  
  .reduce((sum, u) => sum + u.loginCount, 0)
    Callback: pure (addition)
    Associative: yes → parallelizable
    
  .map(u => u.name)
    Callback: pure (field access)
    Parallelizable: yes
    
  Report object:
    - Created at end
    - Returned immediately
    - Ownership transferred to caller

Streaming opportunity:
  - db.query can stream rows
  - filter can process as rows arrive
  - length/reduce/map can accumulate as rows pass
  - Don't need to materialize full users array
  
Parallelization opportunity:
  - If users is large, filter/reduce/map can parallelize
  - reduce is associative → parallel reduce
  
Ownership:
  - users: consumed by filter, never used again
  - activeUsers: consumed by length/reduce/map fusion
  - report: returned (transferred to caller)
  - No GC needed for intermediates
```

**What the compiler produces (conceptual):**

```rust
// Pseudo-code of generated native code

async fn generateDailyReport() -> Report {
    let mut active_count: i64 = 0;
    let mut total_logins: i64 = 0;
    let mut user_names: Vec<String> = Vec::new();
    
    // Stream from database, process each row immediately
    let stream = db.query_stream("SELECT * FROM users");
    
    // Process in batches for parallelization (if large)
    for batch in stream.chunks(1000) {
        // Parallel process batch
        let batch_results = parallel_map(batch, |user| {
            if days_since(user.last_login) < 30 {
                Some((1, user.login_count, user.name.clone()))
            } else {
                None
            }
        });
        
        // Reduce batch results
        for result in batch_results.flatten() {
            active_count += result.0;
            total_logins += result.1;
            user_names.push(result.2);
        }
    }
    
    Report {
        active_users: active_count,
        total_logins: total_logins,
        user_list: user_names,
    }
}

// Key properties:
// - Single pass through data (streaming)
// - No intermediate User[] array
// - Parallel processing of batches
// - Native i64 for counts (not boxed numbers)
// - Direct field access (known offsets)
// - GC only for final user_names (it escapes)
```

**What you didn't write:**
- No `@parallel` annotation
- No `@streaming` annotation
- No ownership annotations
- No effect annotations
- No special syntax
- Just... normal TypeScript

**What you got:**
- Streaming (started processing before all data fetched)
- Fusion (single pass, no intermediate arrays)
- Parallelization (batched parallel processing)
- Native types (i64 counters)
- Minimal GC (only for escaping data)

---

## TypeScript as Optimization Source

TypeScript isn't just for type checking. It's a goldmine of optimization hints.

### Tagged Unions (Discriminated Unions)

TypeScript's killer feature for us:

```typescript
type Result<T, E> = 
  | { ok: true, value: T }
  | { ok: false, error: E }

type Shape = 
  | { kind: 'circle', radius: number }
  | { kind: 'rect', width: number, height: number }
```

**What we do:** Represent as actual tagged union in memory.

```
// Memory layout for Shape:
struct Shape {
  tag: u8,  // 0 = circle, 1 = rect
  union {
    struct { f64 radius; }           // circle
    struct { f64 width; f64 height; } // rect
  }
}

// Switch on kind:
match shape.tag {
  0 => /* circle path, shape.radius is at known offset */
  1 => /* rect path, shape.width/height at known offsets */
}
```

No vtable. No boxing. Tag check is one byte comparison. This is as fast as C.

### Literal Types

```typescript
type HttpMethod = "GET" | "POST" | "PUT" | "DELETE"
type Status = 200 | 201 | 400 | 404 | 500

function handle(method: HttpMethod, status: Status) { ... }
```

**What we do:**
- `HttpMethod` → 4 interned strings, equality is pointer compare
- Or better: compile to enum `{ GET=0, POST=1, PUT=2, DELETE=3 }`
- `Status` → native integers, switch is jump table

```typescript
// User writes:
if (method === "GET") { ... }

// We generate:
if (method == 0) { ... }  // Direct integer compare
```

### Tuple Types

```typescript
type Point = [number, number]
type RGB = [number, number, number]

const p: Point = [10, 20]
```

**What we do:** Not an array. A struct.

```
// Memory:
struct Point { f64 x; f64 y; }

// Access:
p[0] → p.x (offset 0)
p[1] → p.y (offset 8)
// Bounds check? No. Type says exactly 2 elements.
```

### `as const` (Const Assertions)

```typescript
const CONFIG = {
  apiUrl: "https://api.example.com",
  timeout: 5000,
  retries: 3
} as const
```

**What we do:** Compile-time constant. Inline everywhere.

```typescript
// User writes:
fetch(CONFIG.apiUrl, { timeout: CONFIG.timeout })

// After optimization:
fetch("https://api.example.com", { timeout: 5000 })
// String is interned, integers are immediate
```

### Readonly

```typescript
function process(data: readonly number[]): number {
  return data.reduce((a, b) => a + b, 0)
}
```

**What we do:** 
- Data won't be mutated → can share across threads
- No defensive copies needed
- Can parallelize reduce (it's associative + data is immutable)

### Branded/Nominal Types

```typescript
type UserId = string & { readonly __brand: unique symbol }
type OrderId = string & { readonly __brand: unique symbol }

function getUser(id: UserId): User { ... }
```

**What we do:** These are semantically distinct. Could:
- Use different interning pools
- Add debug-mode type tags
- Specialize storage if patterns emerge

### `never` and Exhaustiveness

```typescript
type Action = { type: 'add', value: number } | { type: 'remove', id: string }

function handle(action: Action) {
  switch (action.type) {
    case 'add': return add(action.value)
    case 'remove': return remove(action.id)
    default: 
      const _: never = action  // Compile error if not exhaustive
  }
}
```

**What we do:** 
- Compiler proves switch is exhaustive
- No default branch needed in generated code
- Dead code after `never` assertion eliminated

### Template Literal Types

```typescript
type EventName = `on${Capitalize<string>}`
type Route = `/${string}` | `/${string}/${string}`
```

**What we do:** Pattern constraints on strings. Can specialize:
- `EventName` always starts with "on" → skip those chars in comparisons
- `Route` always starts with "/" → known prefix

---

## Learning from Other Engines

### From V8 (JavaScript)

**Hidden Classes (Shapes):**
V8 discovers shapes at runtime, creates "maps" dynamically. 
We compute shapes at compile time. No runtime shape checks for typed code.

**Inline Caches:**
V8 caches property lookups, starts monomorphic, goes polymorphic/megamorphic.
We resolve at compile time. Property access is offset load. Always monomorphic.

**Smi (Small Integer):**
V8 uses tagged pointers, small integers stored inline.
We use unboxed i32/i64 when proven integer. No tagging overhead.

**Packed vs Holey Arrays:**
V8 distinguishes PACKED_SMI_ELEMENTS, PACKED_DOUBLE_ELEMENTS, etc.
We analyze at compile time: `number[]` that's homogeneous → PackedF64.

**TurboFan Sea of Nodes:**
V8's optimizing compiler uses sea-of-nodes IR (nodes float, scheduled late).
Consider for our MIR—enables more reordering, better instruction selection.

### From PyPy (Python)

**Storage Strategies:**
PyPy uses specialized storage for lists:
- `IntegerListStrategy` for `[1, 2, 3]`
- `FloatListStrategy` for `[1.0, 2.0, 3.0]`
- `ObjectListStrategy` for mixed

We do this statically. TypeScript tells us the element type.

**Escape Analysis:**
PyPy's escape analysis is excellent. Objects that don't escape → stack.
We do this + scalar replacement (split object into registers).

**Guards and Deoptimization:**
PyPy inserts guards, deoptimizes on failure.
We don't need guards for typed code. Types are guaranteed.

### From Ruby (TruffleRuby, YJIT)

**Object Shapes:**
Ruby 3.2+ has shapes like V8, but simpler.
We have shapes at compile time, even simpler.

**Lazy Compilation:**
YJIT compiles methods on first hot call.
We compile everything ahead of time. No warmup.

### From Java (HotSpot, GraalVM)

**Tiered Compilation:**
HotSpot: interpreter → C1 (fast compile) → C2 (slow, optimized)
We could: fast compile for dev → full optimize for release

**Escape Analysis + Scalar Replacement:**
HotSpot/Graal are very good at this. 
```java
Point p = new Point(x, y);
double len = Math.sqrt(p.x*p.x + p.y*p.y);
// Point allocation eliminated, p.x and p.y are registers
```
We do the same, but statically proven (not speculative).

**Lock Elision:**
HotSpot removes locks when object doesn't escape.
We can eliminate synchronization when data is provably unshared.

**Partial Escape Analysis:**
Graal's PEA: object only escapes on some paths → allocate lazily.
We can do this: only allocate if branch that escapes is taken.

---

## Radical Representation Ideas

### Struct-of-Arrays (SOA) for Typed Arrays

```typescript
interface Point { x: number; y: number }
const points: Point[] = [...]
```

**Traditional (AOS - Array of Structs):**
```
[{x,y}, {x,y}, {x,y}, ...]
Memory: x0 y0 x1 y1 x2 y2 ...
```

**SOA - Struct of Arrays:**
```
{ xs: [x0, x1, x2, ...], ys: [y0, y1, y2, ...] }
Memory: x0 x1 x2 ... y0 y1 y2 ...
```

**Why SOA:**
- SIMD: load 4 x's at once, 4 y's at once
- Cache: if you only access x, don't load y
- Vectorization: operations on all x's can vectorize

**When to use:**
- Large arrays (>1000 elements)
- Access patterns favor one field
- SIMD operations on field

Compiler chooses layout based on usage analysis.

### Small Object Optimization

Like small string optimization, but for objects:

```typescript
interface Small { a: number; b: number }  // 16 bytes
```

**Optimization:** Inline in containing struct, not pointer.

```
// Before (pointer):
struct Container { Small* small; ... }

// After (inline):
struct Container { f64 small_a; f64 small_b; ... }
```

**When:** Object is small, doesn't escape, used by one owner.

### Region-Based Memory

For request handlers, pure functions:

```typescript
async function handleRequest(req: Request): Promise<Response> {
  const data = parseBody(req)        // Allocate in region
  const validated = validate(data)    // Allocate in region
  const result = process(validated)   // Allocate in region
  return Response.json(result)        // result escapes, promote
}
// End of function: entire region freed at once
```

**Implementation:**
- Bump allocator for region
- Track what escapes (return value, stored to global)
- Escaping objects promoted to GC heap
- Everything else freed in bulk

**Benefit:** Near-zero GC overhead for request-scoped allocations.

### Interned Everything

Not just strings. Any immutable value:

```typescript
const config = { api: "...", timeout: 5000 } as const
const repeated = Array(1000).fill(config)
```

**Optimization:** `config` is immutable → intern it. All 1000 references point to same object. One allocation, not 1000.

**Extend to:**
- Immutable objects (frozen, or proven unmodified)
- Function results (memoization as interning)
- Common constants (empty array, empty object)

### Value Types (Unboxed Structs)

```typescript
interface Vec3 { x: number; y: number; z: number }

function dot(a: Vec3, b: Vec3): number {
  return a.x * b.x + a.y * b.y + a.z * b.z
}
```

**Current JS:** a and b are heap objects, GC tracked.

**Value type:** Pass by value (copy 24 bytes), no allocation.

```
// Generated:
double dot(double ax, double ay, double az, double bx, double by, double bz) {
  return ax*bx + ay*by + az*bz;
}
```

**Criteria for value type:**
- Small (≤ 64 bytes? tunable)
- Immutable (or copy-on-write)
- Doesn't escape (or escapes into another value type)

### Specialization Cloning

```typescript
const numbers = [3, 1, 4, 1, 5, 9, 2, 6]
numbers.sort((a, b) => a - b)
```

**Generic sort:** Calls comparator function per comparison.
**Specialized sort:** Inline `a - b`, use native comparison, vectorize.

```
// Generated for numeric ascending sort:
// Use SIMD sorting network for small arrays
// Use pdqsort with inlined comparison for large
```

**Other specializations:**
- `array.map(x => x * 2)` → SIMD multiply
- `array.filter(x => x > 0)` → SIMD comparison + compress
- `array.indexOf(needle)` → SIMD search

### Partial Evaluation

If some inputs are known at compile time, specialize:

```typescript
const REGEX = /^[a-z]+$/
function validate(s: string): boolean {
  return REGEX.test(s)
}
```

**Partial evaluation:** REGEX is constant → compile regex to native matcher at compile time.

```typescript
// Generated (conceptually):
function validate(s: string): boolean {
  for each char c in s:
    if c < 'a' || c > 'z': return false
  return s.length > 0
}
```

**Apply to:**
- Constant regex → compiled DFA
- Constant format strings → specialized formatter
- Constant JSON schemas → specialized parser
- Constant SQL → prepared statement structure

---

## Core Philosophy

### Preserve High-Level Semantics

Traditional compilers lower early. AST → IR → machine code. Information is lost at each step.

We do the opposite: **preserve semantics as long as possible**.

When we see `arr.map(f).filter(g).reduce(h)`, we don't immediately lower to loops. We keep it as:

```
Chain(arr, [Map(f), Filter(g), Reduce(h)])
```

This enables:
- Loop fusion (single pass)
- Vectorization (if f, g, h permit)
- Parallelization (if pure)
- Allocation elimination (no intermediate arrays)

When we see `Promise.all([a(), b(), c()])`, we don't lower to sequential execution with a counter. We keep it as:

```
ParallelAwait([Call(a), Call(b), Call(c)])
```

The user wrote `Promise.all`. That's a **semantic signal**: "I don't care about order." We can actually parallelize.

### Use ALL Domain Knowledge

We don't treat APIs as opaque functions. We know what they *mean*.

```
node:fs.writeFile(path, data)
  → Effect: Write(FileSystem, path)
  → Semantics: WriteFile { path, data }
  → Properties: async, may_throw, idempotent(for same data)

Array.prototype.map(fn)
  → Semantics: Map
  → Properties: 
    - pure_if(fn is pure)
    - parallelizable_if(fn is pure, no index dependence)
    - output_length == input_length
    - element_type(output) = return_type(fn)

String.prototype.toLowerCase()
  → Semantics: ToLowerCase
  → Properties:
    - pure, no_throw
    - length_preserving_if(ascii)
    - ascii_preserving_if(ascii input)
```

This knowledge enables:
- Effect analysis without seeing implementation
- Optimization patterns (map fusion, parallel map)
- Type refinement (knowing output type from input)
- Encoding optimization (ASCII stays ASCII through toLowerCase)

### Parallelization: Only When Clear Wins

Most micro-parallelization is a **loss**. Thread spawn overhead, cache coherency, synchronization—these cost thousands of cycles. Parallelizing two additions is absurd.

**Never parallelize:**
- Small computations (< ~10k cycles)
- Sequential dependencies
- Shared mutable state

**Parallelize when:**
- **Explicit signal**: `Promise.all`, `parallel` hints
- **Large independent work**: Network I/O, CPU-bound loops with substantial bodies
- **Proven benefit**: Cost model says gain > overhead

`Promise.all([fetch(a), fetch(b), fetch(c)])` is perfect: user said parallel, work is substantial (network I/O), no dependencies. Execute truly in parallel.

```typescript
// Good parallelization target:
const results = await Promise.all(
  urls.map(url => fetch(url).then(r => r.json()))
);
// User said Promise.all, each fetch is independent I/O

// Bad parallelization target:
const sum = a + b + c + d;
// Parallelizing additions is insane overhead
```

---

## Goal

Transform TypeScript into native code that performs like hand-written Rust/C++.

Target outcomes for well-typed code:
- Strictly typed structs with known layouts (no property bags)
- Raw contiguous arrays (no sparse arrays, no boxed elements)
- Zero GC for static lifetimes (stack allocation, scalar replacement)
- SIMD vectorization where applicable
- Aggressive inlining (zero call overhead for small functions)
- Full monomorphization (no polymorphic dispatch for concrete types)
- Native integer/float operations (no boxing, no NaN checks)
- Cache-friendly memory layouts
- Parallel execution where semantically safe

If the code is well-typed, we should match or beat V8's optimized output for hot code, while being fast from the first call (no warmup).

---

## Architecture Overview

```
┌──────────────────────────────────────────────────────────────────┐
│                    SEMANTIC KNOWLEDGE BASE                        │
├──────────────────────────────────────────────────────────────────┤
│  API semantics, effect signatures, optimization patterns          │
│  ├─ Core JS (Array, String, Promise, Math, ...)                  │
│  ├─ Node.js (fs, http, path, crypto, ...)                        │
│  ├─ Web APIs (fetch, DOM, URL, ...)                              │
│  └─ Ecosystem (popular npm packages)                              │
└──────────────────────────────────────────────────────────────────┘
                              ↓ (consulted throughout)
┌──────────────────────────────────────────────────────────────────┐
│                         FRONTEND                                  │
├──────────────────────────────────────────────────────────────────┤
│  Source (.ts/.js)                                                │
│       ↓                                                          │
│  [Parser]                                                        │
│       ↓                                                          │
│  AST + Type Info                                                 │
│       ↓                                                          │
│  [HIR Lowering] (preserve high-level ops!)                       │
│       ↓                                                          │
│  HIR (typed, effect-annotated, high-level)                       │
│    - Map/Filter/Reduce as first-class ops                        │
│    - Promise.all preserved (not lowered to loops)                │
│    - Async preserved (not lowered to state machine yet)          │
└──────────────────────────────────────────────────────────────────┘
                              ↓
┌──────────────────────────────────────────────────────────────────┐
│                      ANALYSIS ENGINE                              │
├──────────────────────────────────────────────────────────────────┤
│  Whole-Program Analysis (parallel, incremental)                  │
│  ├─ Type Flow Analysis                                           │
│  ├─ Shape Analysis                                               │
│  ├─ Escape Analysis                                              │
│  ├─ Effect Analysis (uses semantic knowledge)                    │
│  ├─ Alias Analysis                                               │
│  ├─ Range Analysis                                               │
│  ├─ Purity Analysis (uses semantic knowledge)                    │
│  ├─ Nullability Analysis                                         │
│  ├─ Integer Detection                                            │
│  ├─ Array Homogeneity Analysis                                   │
│  ├─ String Encoding Analysis                                     │
│  ├─ Sealed Type Detection                                        │
│  ├─ Lifetime Analysis                                            │
│  └─ Parallelizability Analysis                                   │
└──────────────────────────────────────────────────────────────────┘
                              ↓
┌──────────────────────────────────────────────────────────────────┐
│                    OPTIMIZATION PASSES                            │
├──────────────────────────────────────────────────────────────────┤
│  High-Level (HIR → HIR)                                          │
│  ├─ Dead Code Elimination                                        │
│  ├─ Constant Propagation                                         │
│  ├─ Constant Folding                                             │
│  ├─ Copy Propagation                                             │
│  ├─ Common Subexpression Elimination                             │
│  ├─ Devirtualization                                             │
│  ├─ Inlining                                                     │
│  ├─ Monomorphization                                             │
│  ├─ Allocation Sinking                                           │
│  ├─ Allocation Elimination (Scalar Replacement)                  │
│  ├─ Loop Invariant Code Motion                                   │
│  ├─ Loop Unrolling                                               │
│  ├─ Loop Fusion                                                  │
│  ├─ Strength Reduction                                           │
│  ├─ Algebraic Simplification                                     │
│  ├─ Reassociation                                                │
│  ├─ Null Check Elimination                                       │
│  ├─ Bounds Check Elimination                                     │
│  ├─ Type Check Elimination                                       │
│  ├─ Exception Path Pruning                                       │
│  ├─ Async Elision                                                │
│  └─ Parallel Region Formation                                    │
├──────────────────────────────────────────────────────────────────┤
│  Mid-Level (MIR)                                                 │
│  ├─ Memory to Register Promotion                                 │
│  ├─ Load/Store Elimination                                       │
│  ├─ Write Barrier Elimination                                    │
│  ├─ GC Safepoint Optimization                                    │
│  ├─ Stack Allocation Conversion                                  │
│  ├─ Layout Optimization                                          │
│  └─ Vectorization                                                │
└──────────────────────────────────────────────────────────────────┘
                              ↓
┌──────────────────────────────────────────────────────────────────┐
│                         BACKEND                                   │
├──────────────────────────────────────────────────────────────────┤
│  LIR Generation                                                  │
│       ↓                                                          │
│  LLVM IR Emission                                                │
│       ↓                                                          │
│  LLVM Optimization (O3 + LTO)                                    │
│       ↓                                                          │
│  Native Code                                                     │
└──────────────────────────────────────────────────────────────────┘
                              ↓
┌──────────────────────────────────────────────────────────────────┐
│                         RUNTIME                                   │
├──────────────────────────────────────────────────────────────────┤
│  ├─ GC (generational, precise, concurrent mark)                  │
│  ├─ Allocator (bump + free-list hybrid)                          │
│  ├─ String Interner                                              │
│  ├─ Parallel Scheduler (work-stealing)                           │
│  ├─ Async Runtime (stackless coroutines)                         │
│  └─ Standard Library                                             │
└──────────────────────────────────────────────────────────────────┘
```

---

## Existing Codebase

We have substantial infrastructure in `vendor/ecma-rs/`. Assessment of what to keep, extend, or replace:

### Keep (solid foundations)

```
parse-js/
  Full JS/TS/JSX/TSX parser
  Produces typed AST with spans
  Well-tested against Test262
  → Keep as-is. Don't rewrite parsers.

typecheck-ts/
  TypeScript type checker
  Produces TypeId for expressions
  Handles generics, inference, narrowing
  → Keep. Use for type information feed.

semantic-js/
  Scope/symbol binding
  JS and TS modes
  Deterministic symbol IDs
  → Keep. Extend with effect tracking.

hir-js/
  High-level IR from parse-js
  DefId, BodyId, ExprId, PatId
  Lowering from AST
  → Keep structure. EXTEND with high-level ops.

optimize-js/
  IL with SSA form
  CFG construction
  Dataflow analysis framework
  Liveness, defs, loop detection
  Basic optimization passes (DCE, DVN, CFG pruning)
  → Keep dataflow infrastructure. EXTEND with effect/ownership analysis.
```

### Extend (good bones, needs more)

```
hir-js/src/hir.rs
  Current: Traditional AST-like structure (Expr, Stmt, etc.)
  Need: High-level ops (ArrayMap, ArrayFilter, PromiseAll, etc.)
  Plan: Add new ExprKind variants for semantic operations
  
  Current ExprKind has:
    Call, Member, Binary, Unary, etc.
  
  Add:
    ArrayMap { array: ExprId, callback: ExprId }
    ArrayFilter { array: ExprId, callback: ExprId }
    ArrayReduce { array: ExprId, callback: ExprId, init: Option<ExprId> }
    ArrayChain { array: ExprId, ops: Vec<ArrayChainOp> }
    PromiseAll { promises: Vec<ExprId> }
    PromiseRace { promises: Vec<ExprId> }
    JsonParse { input: ExprId, target_type: Option<TypeId> }
    // etc.

optimize-js/src/analysis/
  Current: Dataflow framework, liveness, defs
  Need: Effect analysis, purity analysis, escape analysis, ownership
  Plan: Add new analysis passes using existing dataflow infrastructure
  
  Add:
    effect.rs      - EffectSet per expression
    purity.rs      - Pure/ReadOnly/Impure classification
    escape.rs      - NoEscape/ArgEscape/GlobalEscape
    ownership.rs   - Owned/Borrowed/Shared inference
    encoding.rs    - String encoding tracking (Ascii/Utf8/Unknown)
    range.rs       - Integer range analysis
    nullability.rs - NonNull/MaybeNull tracking

optimize-js/src/il/inst.rs
  Current: Basic IL instructions (Bin, Un, Call, etc.)
  Need: Effect annotations, type info, ownership
  Plan: Extend Inst with metadata
  
  Add to Inst:
    effects: EffectSet
    result_type: TypeInfo
    ownership: OwnershipState
```

### Not Used By This Project (but kept)

```
vm-js/
  Current: Full JavaScript interpreter
           Mark/sweep GC, handles, builtins
           Spec-compliant execution
  Our plan: Native compilation, not interpretation
  Status: KEEP. Used by other projects (browser rendering, etc.)
          May reference for spec behavior and as test oracle.

emit-js/
  Current: Emit JavaScript source (for minifier)
  Our plan: Emit LLVM IR
  Status: KEEP. Used by minify-js and other tooling.
          Not part of native compilation pipeline.

minify-js/
  Current: JavaScript minifier
  Our plan: Native compilation
  Status: KEEP. Separate concern, different use case.
```

### New Components Needed

```
NEW: effect-js/
  Effect inference engine
  API semantic database
  Pattern recognition
  
NEW: native-js/
  LLVM IR generation
  GC integration (statepoints)
  Runtime codegen

NEW: runtime-native/
  Native runtime library
  GC implementation
  Parallel scheduler
  Async runtime
  Standard library (native implementations)

NEW: knowledge-base/
  API semantic definitions (YAML/TOML)
  Node.js APIs
  Web APIs
  Core JS semantics
```

Developer docs for the native TS→LLVM pipeline:

- [`native-js/README.md`](./native-js/README.md)
- [`native-js-cli/README.md`](./native-js-cli/README.md)

---

## Phase 1: Foundation

### 1.1 Parser Integration

Use existing `parse-js`. It's battle-tested against Test262.

```
Existing: parse-js/
  - Full JS/TS/JSX/TSX support
  - Dialect enum: Js, Jsx, Ts, Tsx, Dts
  - SourceType: Module, Script
  - Produces Node<T> with spans and assoc data
  
Pipeline:
  Source → parse_js::parse() → TopLevel AST
                                    ↓
                            hir_js::lower_file()
                                    ↓
                               HirFile + Bodies

No changes needed to parser. It works.
```

### 1.2 HIR Design

Extend existing `hir-js` to **preserve semantic information**. This is key: don't lower early.

```
Existing hir-js structure:

  HirFile {
    file: FileId,
    root_body: BodyId,
    items: Vec<DefId>,
    bodies: Vec<BodyId>,
    imports: Vec<Import>,
    exports: Vec<Export>,
  }
  
  Body {
    kind: BodyKind,
    params: Vec<Param>,
    stmts: Vec<Stmt>,
    exprs: ExprArena,
    pats: PatArena,
  }
  
  ExprKind (existing):
    Literal, Identifier, Binary, Unary, Call, Member,
    Array, Object, Function, Class, Conditional, etc.
```

```
HIR Features:
- SSA form (every value defined once)
- Explicit types on every value
- HIGH-LEVEL OPERATIONS PRESERVED (map, filter, reduce, Promise.all)
- Explicit effects on every expression
- Explicit null/undefined handling
- Function references (for call graph)
- Allocation sites tagged (for escape analysis)

HIR Types:
  Void | Never
  Bool
  I32 | I64 | F64                      // Numeric (proven integer vs float)
  String(StringKind, Encoding)          // Kind: Interned | Heap | Inline
                                        // Encoding: Ascii | Latin1 | Utf8 | Unknown
  Symbol
  Object(ShapeId)
  Array(ElementType, Homogeneity)
  Function(SigId)
  Union(Vec<Type>)
  Nullable(Type)

HIR Ops - LOW-LEVEL (subset):
  // Values
  Const(Literal)
  Param(Index)
  Phi(Vec<(Block, Value)>)
  
  // Arithmetic (type-specific)
  AddI32(Value, Value)
  AddF64(Value, Value)
  MulI32(Value, Value)
  // ... etc
  
  // Memory
  Alloc(ShapeId)
  FieldLoad(Value, FieldId)
  FieldStore(Value, FieldId, Value)
  ArrayLoad(Value, Value)
  ArrayStore(Value, Value, Value)
  
  // Control
  Call(FuncId, Vec<Value>)
  IndirectCall(Value, Vec<Value>)
  Branch(Cond, TrueBlock, FalseBlock)
  Jump(Block)
  Return(Value)
  
  // Checks (can be eliminated by analysis)
  NullCheck(Value)
  BoundsCheck(Array, Index)
  TypeCheck(Value, Type)

HIR Ops - HIGH-LEVEL (the key insight):
  // Array operations - NOT loops yet!
  ArrayMap(Array, Closure)              // arr.map(f)
  ArrayFilter(Array, Closure)           // arr.filter(f)
  ArrayReduce(Array, Closure, Init)     // arr.reduce(f, init)
  ArrayFind(Array, Closure)             // arr.find(f)
  ArrayEvery(Array, Closure)            // arr.every(f)
  ArraySome(Array, Closure)             // arr.some(f)
  
  // Chained operations - enables fusion
  ArrayChain(Array, Vec<ArrayOp>)       // arr.map(f).filter(g).reduce(h)
  
  // Async operations - NOT state machines yet!
  Await(Promise)                        // await p
  PromiseAll(Vec<Promise>)              // Promise.all([...]) - PARALLELIZABLE!
  PromiseRace(Vec<Promise>)             // Promise.race([...])
  AsyncCall(FuncId, Args)               // async function call
  
  // String operations - preserve for encoding analysis
  StringConcat(Vec<String>)             // a + b + c (not incremental)
  StringTemplate(Parts, Exprs)          // `${a} ${b}`
  StringSlice(String, Start, End)       // str.slice(a, b) - view possible
  
  // Object operations
  ObjectSpread(Vec<Object>)             // { ...a, ...b } - enables shape analysis
  ObjectDestructure(Object, Fields)     // const { x, y } = obj
  ArrayDestructure(Array, Count)        // const [a, b] = arr
  
  // Known API calls - semantic info attached
  KnownCall(ApiId, Args)                // Call to known API (fs.readFile, etc.)
                                        // Carries semantic info from knowledge base
```

### 1.2.1 Why High-Level Ops Matter

Traditional approach:
```typescript
// Source
arr.map(f).filter(g).reduce(h, 0)

// Lowered to (too early!):
let t1 = []
for (let i = 0; i < arr.length; i++) t1.push(f(arr[i]))
let t2 = []
for (let i = 0; i < t1.length; i++) if (g(t1[i])) t2.push(t1[i])
let t3 = 0
for (let i = 0; i < t2.length; i++) t3 = h(t3, t2[i])

// Now we have 3 loops, 2 intermediate arrays
// Fusing them requires complex loop analysis
```

Our approach:
```
// HIR
ArrayChain(arr, [Map(f), Filter(g), Reduce(h, 0)])

// This IS the IR. We haven't lowered.
// Optimization pass trivially fuses to single loop.
// Lowering happens AFTER optimization.
```

Same for async:
```typescript
// Source
await Promise.all([fetch(a), fetch(b), fetch(c)])

// Traditional lowering:
// Complex state machine, sequential promise handling

// Our HIR:
PromiseAll([AsyncCall(fetch, a), AsyncCall(fetch, b), AsyncCall(fetch, c)])

// We KNOW this is parallelizable. User said Promise.all.
// Lower to actual parallel execution.
```

### 1.3 Shape System

Static shape analysis replaces runtime hidden classes.

```
Shape:
  id: ShapeId
  name: String                    // Debug only
  fields: Vec<(FieldName, Type, Offset, Mutable)>
  methods: Vec<(MethodName, FuncId)>
  size: u32                       // Total byte size
  align: u32                      // Alignment requirement
  
Constraints:
- All fields at fixed offsets (no hash lookup)
- Compatible shapes share field offsets for shared properties
- Methods can be devirtualized when shape is known
- Shapes are immutable after construction (no prototype mutation)

Interface Unification:
- Types implementing same interface get consistent layout for shared fields
- x.foo always at same offset regardless of concrete type
- Enables polymorphic code without dispatch overhead
```

---

## Phase 2: Analysis Engine

All analyses run on whole program. Parallel where possible. Results cached and incrementally updated.

### 2.1 Type Flow Analysis

Refine types beyond what TypeScript provides.

```
Goals:
- Narrow unions to concrete types at use sites
- Detect integer-only number usage
- Detect homogeneous arrays
- Propagate type constraints through control flow

Algorithm:
- Forward dataflow on SSA
- Meet operation: type intersection
- Iterate to fixpoint
- Track type at each program point

Example:
  function f(x: number | string) {
    if (typeof x === 'number') {
      // x is narrowed to number
      return x + 1;  // AddF64, not polymorphic add
    }
  }
```

### 2.2 Escape Analysis

Determine where allocations can live.

```
Escape States:
  NoEscape        // Can stack allocate or scalar replace
  ArgEscape(i)    // Escapes via argument i (interprocedural)
  ReturnEscape    // Escapes via return (caller decides)
  GlobalEscape    // Must heap allocate
  Unknown         // Conservative

Algorithm:
- Build points-to graph
- Track all uses of each allocation
- Propagate escape state through assignments, calls, returns
- Interprocedural: summarize function effects

Benefits:
  NoEscape → stack allocate (no GC)
  NoEscape + small → scalar replace (eliminate allocation entirely)
  ArgEscape → caller can stack allocate if it doesn't escape there
```

### 2.3 Effect Analysis

Track side effects for reordering and parallelization.

```
Effects:
  reads: Set<Location>
  writes: Set<Location>
  allocates: bool
  throws: Never | Maybe | Always
  diverges: bool
  io: bool

Location:
  Local(VarId)
  Field(ShapeId, FieldName)
  ArrayElement(ArrayType)
  Global(GlobalId)
  Heap                    // Conservative: any heap location

Algorithm:
- Bottom-up on call graph
- Function summaries: effects of calling function
- Expression effects: immediate + transitive through calls

Uses:
- Pure functions can be reordered, memoized, parallelized
- Non-conflicting statements can be parallelized
- Redundant loads can be eliminated
```

### 2.4 Alias Analysis

Determine when two references might point to same object.

```
Precision Levels:
  MustAlias      // Definitely same object
  MayAlias       // Possibly same object
  NoAlias        // Definitely different objects

Algorithm:
- Steensgaard's (fast, imprecise) for initial pass
- Andersen's (slower, precise) for hot code
- Field-sensitive: track fields separately
- Context-sensitive: distinguish call sites

Uses:
- Load elimination: if no alias, load once
- Store sinking: if no alias with intervening loads, move store
- Parallelization: no alias = no conflict
```

### 2.5 Range Analysis

Track numeric value ranges.

```
Range:
  lo: i64 | -∞
  hi: i64 | +∞
  is_integer: bool
  is_non_negative: bool

Algorithm:
- Forward dataflow
- Widen at loop back edges (prevent infinite iteration)
- Narrow at conditionals

Uses:
  is_integer + fits_i32 → use i32 operations
  is_non_negative → eliminate sign checks
  range fits array → eliminate bounds check
  constant range → constant fold
```

### 2.6 Nullability Analysis

Track null/undefined possibility.

```
States:
  NonNull         // Definitely not null/undefined
  MaybeNull       // Might be null
  MaybeUndefined  // Might be undefined
  MaybeBoth       // Might be either

Algorithm:
- Refine through conditionals (if x !== null)
- Propagate through assignments
- Conservative at function boundaries (use declared types)

Uses:
  NonNull → eliminate null checks
  Definitely null path → dead code
```

### 2.7 Purity Analysis

Identify pure functions.

```
Pure if:
  - No writes to anything
  - No I/O
  - No throws (or throws are deterministic)
  - No allocation (debatable—often "pure enough")
  - Terminates

Levels:
  Pure            // No effects, deterministic
  ReadOnly        // Reads but no writes
  Allocating      // Pure except allocation
  Impure          // Has effects

Uses:
  Pure → can memoize, reorder, parallelize, evaluate at compile time
  ReadOnly → can reorder with other reads
```

### 2.8 Lifetime Analysis

Track object lifetimes for GC optimization.

```
Lifetimes:
  Static          // Lives entire program (globals, constants)
  Scoped(Scope)   // Lives for specific scope (locals, temporaries)
  Dynamic         // Unknown, needs GC tracking

Algorithm:
- Combine escape analysis with control flow
- Scoped if NoEscape and all uses in scope
- Static if constant or effectively constant

Uses:
  Scoped → stack allocate, no write barriers
  Static → no GC tracking needed
```

### 2.9 Parallelizability Analysis

Find automatically parallelizable code.

```
Parallel Opportunities:
  Independent statements (no effect conflict)
  Loop iterations (no loop-carried dependencies)
  Map/filter/reduce operations
  Promise.all patterns

Algorithm:
- Build dependency graph using effect analysis
- Find independent subgraphs
- Cost model: only parallelize if benefit > overhead

Output:
  ParallelRegion { statements: Vec<Stmt> }
  ParallelLoop { iterations: Range, body: Block }
```

---

## Phase 2.5: Semantic Knowledge System

The secret weapon. We encode knowledge about APIs, patterns, and semantics.

### 2.5.1 API Semantic Database

Not just type signatures. Semantic meaning.

```
// Node.js fs module
node:fs.readFile:
  semantics: ReadFile
  effects: [IO, Read(FileSystem, path)]
  async: true
  may_throw: true
  pure: false
  idempotent: true  // Same file → same result (modulo external changes)
  
node:fs.writeFile:
  semantics: WriteFile
  effects: [IO, Write(FileSystem, path)]
  async: true
  may_throw: true
  pure: false
  idempotent: false  // Creates file if not exists

node:fs.existsSync:
  semantics: FileExists
  effects: [IO, Read(FileSystem, path)]
  async: false
  may_throw: false
  pure: true  // For same path, same result (snapshot semantics)
  
// HTTP
fetch:
  semantics: HttpRequest
  effects: [IO, Network]
  async: true
  may_throw: true
  pure: false  // Network is not pure
  parallelizable: true  // Independent fetches can parallel

// Array methods
Array.prototype.map:
  semantics: Map
  effects: depends_on(callback)
  pure_if: callback.pure
  parallelizable_if: callback.pure && !callback.uses_index
  output_type: Array<callback.return_type>
  output_length: input.length  // Always same length
  
Array.prototype.filter:
  semantics: Filter
  effects: depends_on(callback)
  pure_if: callback.pure
  output_type: Array<input.element_type>
  output_length: <= input.length
  
Array.prototype.reduce:
  semantics: Reduce
  effects: depends_on(callback)
  pure_if: callback.pure
  associative_if: callback.associative  // Enables parallelization
  
Array.prototype.forEach:
  semantics: ForEach
  effects: depends_on(callback)
  pure: false  // By convention, forEach is for side effects
  return_type: void

// String methods
String.prototype.toLowerCase:
  semantics: ToLowerCase
  effects: []
  pure: true
  throws: never
  properties:
    - length_preserving_if: ascii
    - output_encoding: same_as_input_if(ascii)
    
String.prototype.split:
  semantics: Split
  effects: [Allocates]  // Creates array
  pure: true
  throws: never
  output_type: string[]

// Math
Math.sqrt:
  semantics: Sqrt
  effects: []
  pure: true
  throws: never
  domain: non_negative
  range: non_negative
  
Math.floor:
  semantics: Floor
  effects: []
  pure: true
  throws: never
  output_property: is_integer
  
// JSON
JSON.parse:
  semantics: JsonParse
  effects: [Allocates]
  pure: true
  may_throw: true  // Invalid JSON
  output_type: infer_from_context  // If assigned to typed variable
  
JSON.stringify:
  semantics: JsonStringify
  effects: [Allocates]
  pure: true
  throws: never  // For serializable values
```

### 2.5.2 Pattern Recognition

Recognize high-level patterns that enable optimization.

```
Pattern: MapFilterReduce
  Match: arr.map(f).filter(g).reduce(h, init)
  Rewrite: single_pass_map_filter_reduce(arr, f, g, h, init)
  Benefit: 1 loop instead of 3, no intermediate arrays

Pattern: PromiseAllFetch
  Match: Promise.all(urls.map(url => fetch(url)))
  Semantics: ParallelFetch(urls)
  Optimization: True parallel execution, connection pooling

Pattern: AsyncIterator
  Match: for await (const x of asyncIterable) { ... }
  Semantics: AsyncIteration
  Optimization: Prefetch next while processing current

Pattern: JsonParseTyped
  Match: const x: T = JSON.parse(str)
  Optimization: Generate T-specific parser, skip unused fields

Pattern: StringTemplate
  Match: `${a} ${b} ${c}`
  Optimization: Estimate total length, single allocation

Pattern: ObjectSpread
  Match: { ...a, ...b, x: 1 }
  Optimization: If shapes known, direct field copies

Pattern: ArrayDestructure
  Match: const [a, b, c] = arr
  Optimization: Direct index access, no iterator protocol

Pattern: MapGetOrDefault
  Match: map.has(k) ? map.get(k) : default
  Rewrite: single lookup with default

Pattern: GuardClause
  Match: if (!x) return; // or throw
  Optimization: x is NonNull after this point
```

### 2.5.3 Encoding Analysis

Track string encodings for optimization.

```
StringEncoding:
  Ascii       // All bytes < 128
  Latin1      // All bytes < 256
  Utf8        // General UTF-8
  Unknown     // Conservative

Propagation:
  - String literals: analyze at compile time
  - API returns: 
    - url.pathname → Ascii (URL encoding)
    - number.toString() → Ascii
    - Date.toISOString() → Ascii
  - Operations:
    - toLowerCase(Ascii) → Ascii
    - toUpperCase(Ascii) → Ascii
    - slice(any) → same encoding
    - concat(Ascii, Ascii) → Ascii
    - concat(any, any) → Unknown

Benefits:
  Ascii/Latin1:
    - 1 byte per char (not 2 for UTF-16 like JS)
    - O(1) length, indexing
    - SIMD operations work directly
    - memcmp for equality
```

### 2.5.4 Semantic Signals

User code contains signals about intent. Use them.

```
Signal: Promise.all
  Intent: "These are independent, order doesn't matter"
  Action: Actually parallelize

Signal: async function that never awaits
  Intent: Probably a mistake, or compatibility shim
  Action: Make sync, warn

Signal: const (not let)
  Intent: "I won't reassign"
  Action: More aggressive optimization, can inline value

Signal: readonly modifier
  Intent: "I won't mutate"
  Action: Can share, can cache, no write barriers

Signal: as const
  Intent: "Literal types"
  Action: Compile-time constants

Signal: private fields (#x)
  Intent: "Only this class accesses"
  Action: Can inline, can reorder, no external access

Signal: Type assertion (x as T)
  Intent: "Trust me, it's T"
  Action: Trust (or verify in debug mode)
```

### 2.5.5 Knowledge Base Architecture

```
KnowledgeBase:
  ├── core/
  │   ├── primitives.yaml      # number, string, boolean, etc.
  │   ├── array.yaml           # Array methods
  │   ├── object.yaml          # Object methods
  │   ├── promise.yaml         # Promise, async/await
  │   ├── collections.yaml     # Map, Set, WeakMap, etc.
  │   └── math.yaml            # Math.*
  │
  ├── node/
  │   ├── fs.yaml              # File system
  │   ├── path.yaml            # Path manipulation
  │   ├── http.yaml            # HTTP server/client
  │   ├── crypto.yaml          # Crypto
  │   └── buffer.yaml          # Buffer
  │
  ├── web/
  │   ├── fetch.yaml           # Fetch API
  │   ├── dom.yaml             # DOM APIs
  │   └── url.yaml             # URL API
  │
  └── ecosystem/
      ├── lodash.yaml          # lodash
      ├── rxjs.yaml            # RxJS
      └── ...                  # Community contributed

Format (YAML example):
  name: Array.prototype.map
  signature: <T, U>(arr: T[], fn: (x: T, i: number, arr: T[]) => U) => U[]
  semantics: Map
  effects:
    base: [Allocates]
    callback_dependent: true
  properties:
    pure_if: callback.pure
    parallelizable_if: callback.pure && !callback.uses_index
    output_length: input.length
    fusable_with: [map, filter]
  optimizations:
    - pattern: arr.map(f).map(g)
      rewrite: arr.map(x => g(f(x)))
    - pattern: arr.map(f).filter(g)
      rewrite: single_pass_map_filter(arr, f, g)
```

### 2.5.6 Fallback Behavior

For unknown APIs:

```
Unknown function call:
  - Effects: Conservative (may read/write anything)
  - Purity: Assume impure
  - Throws: Assume may throw
  - Type: Trust declared return type

Unknown object:
  - Shape: Dictionary mode (slow path)
  - Access: Hash lookup

Unknown callback:
  - Effects: Conservative
  - No fusion, no parallelization

Mitigation:
  - Core APIs are always known
  - Popular npm packages get annotations
  - Community can contribute
  - Annotations are versioned with package versions
```

---

## Phase 3: Optimizations

### 3.1 Inlining

Aggressive function inlining.

```
Heuristics:
- Always inline: leaf functions < 10 instructions
- Always inline: functions called once
- Always inline: functions that enable other optimizations (type-specialization)
- Cost model: instruction count, register pressure
- Recursion: partial inlining (inline first N levels)

Special cases:
- Inline through polymorphic calls when type is known
- Inline closures when possible
- Inline getters/setters (often trivial)
```

### 3.2 Monomorphization

Specialize generic/polymorphic code for concrete types.

```
Strategy:
- For each call site with known concrete types, generate specialized version
- Share specializations for same type arguments
- Keep polymorphic version as fallback (if needed)

Example:
  function map<T, U>(arr: T[], f: (x: T) => U): U[]
  
  // Called with number[], (x) => x * 2
  // Generate: map_number_number(arr: number[], f: (x: number) => number)
  // Which can use packed f64 arrays, no boxing

Limits:
- Cap specializations per function (prevent code bloat)
- Share when type doesn't affect codegen (e.g., both pointer types)
```

### 3.3 Devirtualization

Replace indirect calls with direct calls.

```
Cases:
  1. Type is exactly known (not interface) → direct call
  2. Single implementation of interface method → direct call
  3. Few implementations → switch + direct calls (guarded devirt)
  
Example:
  interface Drawable { draw(): void }
  // Only Circle and Square implement Drawable
  
  function render(d: Drawable) {
    d.draw();  // Originally: indirect call through vtable
  }
  
  // After devirt:
  function render(d: Drawable) {
    if (d is Circle) { Circle_draw(d); }
    else { Square_draw(d); }
  }
  
  // After inlining (if draw is small):
  function render(d: Drawable) {
    if (d is Circle) { /* Circle draw code */ }
    else { /* Square draw code */ }
  }
```

### 3.4 Allocation Elimination

Remove unnecessary allocations.

```
Scalar Replacement:
- Object doesn't escape
- All field accesses known at compile time
- Replace object with individual variables for each field

Example:
  function distance(x1: number, y1: number, x2: number, y2: number) {
    const p1 = { x: x1, y: y1 };  // Allocation
    const p2 = { x: x2, y: y2 };  // Allocation
    const dx = p2.x - p1.x;
    const dy = p2.y - p1.y;
    return Math.sqrt(dx * dx + dy * dy);
  }
  
  // After scalar replacement:
  function distance(x1, y1, x2, y2) {
    // p1_x = x1, p1_y = y1, p2_x = x2, p2_y = y2
    const dx = x2 - x1;
    const dy = y2 - y1;
    return Math.sqrt(dx * dx + dy * dy);
  }
  
  // Zero allocations!

Stack Allocation:
- Object doesn't escape function
- Too complex for scalar replacement
- Allocate on stack instead of heap
```

### 3.5 Bounds Check Elimination

Remove redundant array bounds checks.

```
Cases:
  1. Index is constant and within bounds → eliminate
  2. Loop with index 0..len → eliminate checks in loop body
  3. Previous check dominates → eliminate
  4. Range analysis proves in-bounds → eliminate

Example:
  for (let i = 0; i < arr.length; i++) {
    arr[i] = 0;  // i is always in bounds
  }
  
  // After:
  for (let i = 0; i < arr.length; i++) {
    arr[i] = 0;  // No bounds check
  }
```

### 3.6 Null Check Elimination

Remove redundant null checks.

```
Cases:
  1. Value just constructed → not null
  2. Just checked → dominated uses are not null
  3. Type doesn't include null → not null
  4. Non-null assertion (!) with type system proof → eliminate check

Example:
  function process(x: Foo | null) {
    if (x === null) return;
    // Here x is proven non-null
    x.bar();  // No null check needed
    x.baz();  // No null check needed
  }
```

### 3.7 Loop Optimizations

```
Loop Invariant Code Motion (LICM):
- Move computations that don't change across iterations out of loop
- Requires purity analysis (no side effects)

Loop Unrolling:
- Known iteration count + small body → fully unroll
- Unknown count → partial unroll (2x, 4x, 8x)
- Enables further optimizations on unrolled code

Loop Fusion:
- Adjacent loops over same range → combine into one
- Reduces loop overhead, improves locality

Loop Interchange:
- Nested loops: swap order for better memory access pattern
- Row-major vs column-major traversal

Induction Variable Simplification:
- Replace expensive operations (mul) with cheaper ones (add)
- i * stride → accumulator += stride
```

### 3.8 Advanced Loop Optimization

Beyond basic loop transforms, leverage LLVM's advanced capabilities:

```
Polly (LLVM's Polyhedral Optimizer):
  - Automatic loop tiling (cache blocking)
  - Loop fusion across non-adjacent loops
  - Loop interchange for memory access patterns
  - Automatic parallelization of loop nests
  
When to use:
  - Nested loops over arrays
  - Matrix/tensor operations
  - Image processing patterns
  
Example:
  // Original: poor cache behavior
  for (let i = 0; i < N; i++)
    for (let j = 0; j < N; j++)
      C[i][j] = A[i][j] + B[i][j]
  
  // After tiling: cache-friendly
  for (let ii = 0; ii < N; ii += TILE)
    for (let jj = 0; jj < N; jj += TILE)
      for (let i = ii; i < min(ii+TILE, N); i++)
        for (let j = jj; j < min(jj+TILE, N); j++)
          C[i][j] = A[i][j] + B[i][j]

Integration:
  - Emit LLVM IR with loop metadata
  - Enable Polly passes for numeric-heavy code
  - Let Polly handle cache optimization automatically
```

### 3.9 Vectorization

SIMD for data-parallel operations.

```
Targets:
- Homogeneous numeric arrays
- Map/filter/reduce patterns
- Known iteration counts (or multiples of vector width)

Example:
  function dotProduct(a: number[], b: number[]): number {
    let sum = 0;
    for (let i = 0; i < a.length; i++) {
      sum += a[i] * b[i];
    }
    return sum;
  }
  
  // Vectorized (conceptually):
  function dotProduct(a: f64[], b: f64[]): f64 {
    let sum = f64x4(0, 0, 0, 0);
    for (let i = 0; i < a.length; i += 4) {
      sum += f64x4.load(a, i) * f64x4.load(b, i);
    }
    return sum.horizontal_add() + scalar_remainder;
  }

Requirements:
- Prove array is PackedF64 (homogeneous)
- Alignment info (for aligned loads)
- No aliasing between a and b (or prove same)
```

### 3.9 Async Elision

Remove async overhead when not needed.

```
Cases:
  1. Async function never awaits → make sync
  2. Await on already-resolved promise → eliminate
  3. Single await at end → tail-call optimization
  4. All awaits on sync values → eliminate all

Example:
  async function fetchAndProcess(cached: boolean) {
    if (cached) {
      return cachedResult;  // No await needed
    }
    return await fetch(url);
  }
  
  // Split into:
  function fetchAndProcess(cached: boolean) {
    if (cached) {
      return cachedResult;  // Sync path
    }
    return fetchAndProcess_async(url);  // Async only when needed
  }
```

### 3.10 Exception Path Pruning

Eliminate unreachable exception handling.

```
Analysis:
- Track which functions can throw
- Track which operations can throw
- Propagate through call graph

Optimization:
- If path to catch is impossible → eliminate catch
- If function can't throw → eliminate try wrapper
- If exception type can't occur → eliminate that handler
```

---

## Phase 4: Data Representations

### 4.1 Numbers

```
Representation:
  I32   → raw 32-bit signed integer
  I64   → raw 64-bit signed integer  
  F64   → raw 64-bit IEEE float

Detection:
- Explicit annotations (x: number but used as integer)
- Range analysis (always in i32 range)
- Pattern analysis (loop indices, array lengths)
- API returns (array.length, string.charCodeAt)

Operations:
  I32 + I32 → I32 (with overflow check or wrap)
  F64 + F64 → F64
  I32 + F64 → F64 (promote)
  
Optimization:
  If proven no overflow: use native add (no check)
  If overflow possible: check and deopt, or promote to i64/f64
```

### 4.2 Strings

```
Representations:
  
  InlineString:
    - Small strings (≤23 bytes UTF-8) stored inline
    - No allocation, no indirection
    - Tagged to distinguish from heap
    
  HeapString:
    - Larger strings on heap
    - Reference counted or GC'd
    - Immutable (can share safely)
    
  InternedString:
    - Strings used as identifiers/keys
    - Stored in global intern table
    - Equality is pointer comparison
    - Perfect for property names, map keys

Small String Optimization:
  struct String {
    union {
      struct { char data[23]; u8 len_and_tag; }  // Inline
      struct { char* ptr; u64 len; u64 cap; }    // Heap
    }
  }
  
  // Tag in high bit of len_and_tag distinguishes

String Interning:
  - Automatically intern string literals
  - Intern strings used as object keys
  - Intern strings compared frequently
  - Hash table with weak refs (GC can collect unused)

String Operations:
  - Concatenation: rope or copy based on size
  - Slice: view into original (no copy)
  - Comparison: length check first, then memcmp
```

### 4.3 Arrays

```
Representations:

  PackedI32:
    - All elements proven i32
    - Contiguous i32[] in memory
    - Direct indexing, no boxing
    
  PackedF64:
    - All elements proven f64
    - Contiguous f64[] in memory
    - Direct indexing, no boxing
    
  PackedObject<Shape>:
    - All elements same shape
    - Contiguous pointers or inline structs
    - Enables vectorization
    
  MixedArray:
    - Heterogeneous elements
    - Tagged union per element
    - Fallback for complex cases

Optimization:
  - Start as packed, degrade if heterogeneous element added
  - Our analysis prevents degradation in proven cases
  - Length stored inline (no indirection)
  - Capacity for growable arrays

Array Operations:
  - Index: bounds check (eliminable) + load
  - Push: capacity check, store, increment length
  - Map/filter/reduce: detect and vectorize
  - Slice: view (no copy) when possible
```

### 4.4 Objects

```
Layout:

  Strict Shape (most objects):
    struct MyObject {
      header: ObjectHeader,  // GC info, shape pointer
      field1: Field1Type,
      field2: Field2Type,
      ...
    }
    
    - Fixed layout, known at compile time
    - Field access is offset load
    - No hash table, no property lookup
    
  Dictionary Mode (rare, discouraged):
    struct DictObject {
      header: ObjectHeader,
      props: HashMap<String, Value>,
    }
    
    - For truly dynamic objects
    - Performance penalty (hash lookup)
    - Try to avoid through analysis

Interface Layout:
  - Types implementing same interface share field offsets
  - interface Point { x: number; y: number }
  - All Point implementations have x at offset 8, y at offset 16
  - Polymorphic access doesn't need dispatch
```

### 4.5 Unions / Enums

```
Representation:

  Small union (≤4 variants):
    struct Union {
      tag: u8,
      data: [u8; max_variant_size],
    }
    // Inline, no allocation
    
  Nullable<T>:
    // If T is pointer: use null pointer
    // If T is non-pointer: option pattern
    struct Nullable<T> {
      has_value: bool,
      value: MaybeUninit<T>,
    }
    // Or: special sentinel value when possible
    
  Tagged pointer (when applicable):
    // Use low bits of pointer for tag
    // Requires alignment guarantees
    
Type narrowing:
  - Switch on tag
  - Each branch knows concrete type
  - Enables type-specific optimizations in each branch
```

### 4.6 Functions / Closures

```
Regular Functions:
  - Compile to native functions
  - Direct call when target known
  - Indirect call through function pointer when not

Closures:
  struct Closure {
    fn_ptr: fn(*ClosureEnv, Args...) -> Ret,
    env: *ClosureEnv,  // Captured variables
  }
  
  Optimization:
  - If closure doesn't capture → just function pointer
  - If closure captured variables are const → inline constants
  - If closure escapes but doesn't mutate captures → copy captures
  - If closure doesn't escape → stack allocate env
  
Closure Inlining:
  - When closure is called immediately and doesn't escape
  - Inline closure body, replace captured vars with actual values
```

---

## Phase 5: Runtime

### 5.1 Memory Allocator

```
Design:
  - Bump allocator for nursery (young generation)
  - Free-list allocator for old generation
  - Large object space for big allocations
  
Nursery:
  - Linear allocation (bump pointer)
  - Extremely fast: just increment pointer
  - Collected by copying to old gen
  
Old Generation:
  - Size-segregated free lists
  - Coalescing for fragmentation
  - Concurrent marking
  
Stack Allocation:
  - Compiler-directed stack allocation for NoEscape objects
  - No GC tracking needed
  - Automatically freed on function return
```

### 5.2 Garbage Collector

```
Design: Immix-inspired, Precise, Generational

Why Immix:
  - Best-in-class for allocation-heavy workloads (JS pattern)
  - Bump-pointer allocation (fast)
  - Mark-region collection (efficient)
  - Opportunistic copying (reduces fragmentation)
  - Good cache locality

Structure:
  Heap organized into:
    - Blocks (32KB, aligned)
    - Lines (128 bytes within blocks)
  
  Allocation:
    - Bump pointer within block
    - When block full, get new block
    - Extremely fast (just increment pointer)
  
  Collection:
    - Mark live objects
    - Reclaim lines that are fully dead
    - Opportunistically copy to defragment
    - Don't move pinned objects (FFI, etc.)

Generations (optional layer on Immix):
  - Nursery: recent blocks, collected frequently
  - Old: survived blocks, collected less often
  - Promotion: objects that survive N collections move to old

Precision:
  - Exact stack maps via LLVM statepoints
  - Know exactly which slots are pointers
  - No conservative scanning (unlike Boehm)

Concurrency:
  - Concurrent marking (application runs during mark)
  - Stop-the-world only for roots + final cleanup
  - Write barriers for old → young pointers
  - Incremental when pressure is low

LLVM Integration:
  @llvm.experimental.gc.statepoint - safepoint insertion
  @llvm.experimental.gc.relocate - pointer relocation
  Stack maps - generated automatically by LLVM

Write Barrier Elimination:
  - NoEscape objects: no barrier needed
  - Young → young: no barrier needed
  - Proven non-pointer field: no barrier needed
  - Compiler eliminates most barriers statically

Safepoints:
  - At loop back-edges (so long loops can be interrupted)
  - At function calls (natural safepoints)
  - Polling: check flag, branch if GC requested
  - Cost: ~1 instruction per safepoint when GC not active
```

### 5.3 String Interner

```
Design:
  - Global hash table of interned strings
  - Lock-free reads (concurrent access)
  - Locked writes (rare after startup)
  
API:
  intern(str: &str) -> InternedId
  lookup(id: InternedId) -> &str
  eq(a: InternedId, b: InternedId) -> bool  // Just ==
  
GC Integration:
  - Weak references in intern table
  - Unused strings collected
  - Common strings (keywords, property names) pinned

Usage:
  - All property names interned
  - String literals interned at compile time
  - Map/Set keys interned automatically
```

### 5.4 Parallel Scheduler

```
Design: Work-stealing thread pool

Components:
  - Fixed number of worker threads (= CPU cores)
  - Per-worker deque (push/pop local, steal remote)
  - Global queue for new work
  
API:
  spawn(task: fn() -> T) -> Future<T>
  spawn_blocking(task: fn() -> T) -> Future<T>  // For I/O
  parallel_for(range, body: fn(i))
  
Integration:
  - Compiler inserts parallel spawn for detected regions
  - Runtime decides actual parallelism (cost model)
  - Granularity control (don't parallelize tiny tasks)

Memory Model:
  - No shared mutable state (enforced by compiler)
  - Message passing for communication
  - Zero-copy when ownership transferred
```

### 5.5 Async Runtime

```
Design: Stackless coroutines + event loop

Coroutines:
  - Async functions compile to state machines
  - State stored in heap-allocated frame (or stack if NoEscape)
  - Explicit yield points (await)
  
Event Loop:
  - epoll/kqueue for I/O
  - Timer wheel for timeouts
  - Integration with parallel scheduler

Optimization:
  - Inline simple async functions
  - Elide async when not needed
  - Fast path for already-resolved promises
```

---

## Phase 6: API Surface

### Philosophy

Three tiers:
1. **Web Standards** — follow spec (fetch, URL, TextEncoder, etc.)
2. **Node APIs** — implement most, pragmatic subset
3. **Our APIs** — where we can do better

Like our JS semantics stance: **spec-compatible where it matters, pragmatically different where it enables wins.**

### Tier 1: Web Standards (Follow Spec)

These are standardized. Follow them.

```
fetch, Request, Response, Headers
URL, URLSearchParams
TextEncoder, TextDecoder
AbortController, AbortSignal
crypto.subtle (WebCrypto)
Blob, File, FormData
ReadableStream, WritableStream, TransformStream
setTimeout, setInterval, queueMicrotask
console
structuredClone
performance.now()
```

**Why follow exactly:**
- Portability (code works in browser too)
- Ecosystem expects it
- Specs are generally well-designed
- Testing against WPT (Web Platform Tests)

**Allowed deviations:**
- Performance characteristics (obviously)
- Streaming behavior (we can be more eager)
- Error message text (not specified)

### Tier 2: Node APIs (Pragmatic Subset)

Node has accumulated cruft. Implement what people actually use.

```
IMPLEMENT (core usage):
  node:fs (promises API primarily)
    readFile, writeFile, readdir, stat, mkdir, rm, rename
    createReadStream, createWriteStream
    watch (FSEvents/inotify)
  
  node:path
    join, resolve, dirname, basename, extname, parse, format
    sep, delimiter
  
  node:os
    platform, arch, cpus, homedir, tmpdir, hostname
  
  node:crypto
    randomBytes, randomUUID, createHash, createHmac
    subtle (WebCrypto, already in Tier 1)
  
  node:buffer
    Buffer (but prefer Uint8Array where possible)
  
  node:child_process
    spawn, exec, execFile
    
  node:http / node:https
    createServer, request
    (but prefer fetch for client)
    
  node:net
    createServer, createConnection, Socket
    
  node:stream
    Readable, Writable, Transform, pipeline
    (but prefer Web Streams where possible)
    
  node:util
    promisify, inspect, format
    
  node:events
    EventEmitter
    
  node:url
    URL, URLSearchParams (Tier 1, re-export)
    parse, format (legacy, for compat)
    
  node:assert
    assert, strictEqual, deepStrictEqual, throws

SKIP (legacy/niche):
  node:cluster (use our parallelization)
  node:domain (deprecated)
  node:punycode (deprecated)
  node:querystring (use URLSearchParams)
  node:readline (niche)
  node:repl (niche)
  node:string_decoder (internal)
  node:tls (use https)
  node:tty (niche)
  node:v8 (obviously)
  node:vm (dynamic code, conflicts with our model)
  node:wasi (niche)
  node:worker_threads (use our parallelization)
  node:zlib (provide, but lower priority)
  
MODIFIED:
  node:fs callbacks → provide but prefer promises
  node:stream → provide but prefer Web Streams
  Buffer → provide but prefer Uint8Array
```

**Deviations allowed:**
- Prefer promises over callbacks (callbacks still work)
- Prefer Web Streams over Node streams
- Error codes/messages may differ slightly
- Timing characteristics (we may be faster or differently ordered)
- `fs.readFileSync` in async context → warning or error (encourages async)

### Tier 3: Our APIs (Where We Do Better)

APIs that leverage our compiler's knowledge.

```typescript
// Parallel utilities (compiler-aware)
import { parallel } from 'std:parallel';

// These LOOK like normal functions but compiler optimizes them
const results = await parallel.map(items, async (item) => {
  return await processItem(item);
});
// Compiler knows: parallel.map = explicit parallelization signal
// Even stronger than Promise.all

// Typed JSON (compiler generates specialized parser)
import { json } from 'std:json';

interface User { name: string; age: number; }
const user = json.parse<User>(input);
// Compiler generates User-specific parser
// Skips unknown fields, direct struct population

// Streaming utilities
import { stream } from 'std:stream';

const results = stream(users)
  .map(enrichUser)
  .filter(isActive)
  .batch(100)  // Process in batches of 100
  .parallel()  // Explicit: parallelize batches
  .collect();
// Explicit streaming pipeline with control

// SQL (if we go there)
import { sql } from 'std:sql';

const users = await sql<User[]>`
  SELECT * FROM users WHERE active = ${true}
`;
// Compiler: type-checks query against schema
// Compiler: generates prepared statement
// Runtime: connection pooling, etc.

// HTTP server with our optimizations
import { serve } from 'std:http';

serve({
  port: 3000,
  async fetch(req) {
    // This is the Web Standard signature
    return new Response('Hello');
  }
});
// But: compiler knows this is a request handler pattern
// Optimizes accordingly

// Typed environment
import { env } from 'std:env';

// env.d.ts declares your env vars
const apiKey: string = env.API_KEY;  // Type-safe, required
const debug: boolean = env.DEBUG ?? false;  // Optional with default
```

### Where We Can Break/Differ

**Small breaks (acceptable):**

```typescript
// IEEE 754 edge cases
-0 === 0  // We might just have one zero

// Object property order
// Spec says insertion order; we might not guarantee in all cases

// typeof null
typeof null === 'object'  // We keep this (too breaking to change)

// Array holes
const arr = [1, , 3];  // We might not support sparse arrays
arr[1]  // undefined in JS, might error for us

// Arguments object
function f() { return arguments; }  // Might not support

// with statement
with (obj) { ... }  // Not supported

// eval, new Function
eval('code')  // Not supported (dynamic code)

// Proxy
new Proxy(...)  // Limited support or unsupported

// Prototype mutation
obj.__proto__ = other;  // Not supported after construction
```

**Medium breaks (documented, opt-out available):**

```typescript
// Await timing
// Spec: await always yields to microtask queue
// Us: if value is ready, might not yield
async function f() {
  const x = await alreadyResolved;
  // In spec: code after here runs in next microtask
  // In us: might run synchronously
}
// 99% of code doesn't care

// Exception ordering in Promise.all
// Spec: first rejection wins
// Us: any rejection, might be different one if parallel
await Promise.all([...]);
// Matters rarely

// Property enumeration order with integer keys
for (const k in obj) { ... }
// Spec has complex rules; we might simplify
```

**Never break:**

```typescript
// Basic arithmetic
1 + 1 === 2  // Obviously

// String operations
'a' + 'b' === 'ab'  // Obviously

// Array basics
arr[0], arr.push(), arr.length  // Core behavior

// Object basics
obj.prop, obj['prop'], 'prop' in obj  // Core behavior

// Equality
===, ==, Object.is()  // Keep semantics

// Control flow
if, for, while, try/catch  // Obviously

// Async/await basic semantics
await returns value, async returns promise  // Core

// this binding
method.call(), method.apply(), arrow functions  // Core
```

### Standard Library Implementation

Optimized implementations of common operations.

### 6.1 Collections

```
Array<T>:
  - Packed representations for primitives
  - Optimized map/filter/reduce (vectorized when possible)
  - Lazy iterators for chaining

Map<K, V>:
  - Swiss table implementation (fast, cache-friendly)
  - Interned keys for string maps
  - Specialized for common key types

Set<T>:
  - Same backing as Map

String operations:
  - SIMD string search
  - Optimized UTF-8 handling
  - Rope for concatenation-heavy use
```

### 6.2 Math

```
- Direct CPU instructions for basic ops
- SIMD for vector math
- Lookup tables for common functions (sin, cos) when precision allows
- Fast inverse sqrt and similar tricks
- Integer fast paths when applicable
```

### 6.3 JSON

```
Typed JSON Parsing:
  - Schema-driven parser (no intermediate representation)
  - Stream directly into typed structs
  - Skip unused fields
  - SIMD for string scanning, number parsing

Example:
  interface User { name: string; age: number; }
  const user: User = JSON.parse(input);
  
  // Generates specialized parser that:
  // - Looks for "name" and "age" keys only
  // - Parses string directly into user.name
  // - Parses number directly into user.age
  // - Skips everything else
```

---

## Phase 7: Tooling

### 7.1 Compiler CLI

```
Commands:
  build           Compile to native binary
  check           Type check without codegen
  run             Compile and run
  bench           Compile with profiling, run benchmarks
  
Flags:
  --release       Full optimization (slow compile, fast code)
  --debug         Debug info, fast compile
  --target        Cross-compilation target
  --emit          Output IR at various stages (hir, mir, llvm)
```

### 7.2 IDE Integration

```
Features:
  - Type information (from our refined types, not just TS)
  - Inlining preview (see what will inline)
  - Allocation annotations (where allocations happen)
  - Optimization hints (what prevents optimization)
  - Performance annotations (predicted cost)
```

### 7.3 Debugging

```
Source Maps:
  - Map native code back to TypeScript
  - Variable mapping (even after scalar replacement)
  - Step through original source

Debug Builds:
  - Minimal optimization for debuggability
  - Bounds checks retained
  - Null checks retained
  - Assertions for type assumptions
```

---

## Optimization Catalog

Every optimization we implement, organized by category.

### Numeric Optimizations

```
Integer Detection
  - Track which numbers are always integers
  - Generate i32/i64 code instead of f64
  - Eliminate float→int conversions

Integer Range Propagation
  - Track min/max bounds
  - Eliminate overflow checks when provably safe
  - Enable bounds check elimination

Strength Reduction
  - x * 2 → x + x or x << 1
  - x / 2 → x >> 1 (for integers)
  - x % 4 → x & 3 (for positive integers)

Fast Division
  - Division by constant → multiply by reciprocal
  - Modulo by power of 2 → bitwise and

Float to Int
  - When result is always truncated, use direct conversion
  - Math.floor(x) where x is positive → trunc
```

### String Optimizations

```
Small String Optimization (SSO)
  - Strings ≤23 bytes inline (no heap allocation)
  - Most strings are small

String Interning
  - Property names, map keys
  - Equality becomes pointer comparison
  - Hash computed once

Rope Concatenation
  - a + b + c + d doesn't create intermediates
  - Build rope, flatten once at end

SIMD String Operations
  - memcmp for equality
  - SIMD search for indexOf
  - Vectorized case conversion
  
Compile-Time String Operations
  - Constant strings evaluated at compile time
  - Template literals with constants folded
```

### Object Optimizations

```
Scalar Replacement
  - NoEscape objects broken into fields
  - Fields become local variables
  - Zero allocation

Stack Allocation
  - NoEscape objects on stack
  - No GC tracking
  - Automatic cleanup

Inline Caching (Not IC)
  - We don't need runtime IC
  - Shapes known at compile time
  - Field access is direct offset

Method Devirtualization
  - Single implementation → direct call
  - Few implementations → type switch
  - Combined with inlining

Field Reordering
  - Pack fields to minimize padding
  - Hot fields together for cache
```

### Array Optimizations

```
Packed Arrays
  - Homogeneous arrays use packed representation
  - i32[], f64[], SameShape[]
  - No boxing overhead

Bounds Check Elimination
  - Loop bounds prove index valid
  - Range analysis proves index valid
  - Hoisting: check once before loop

Vectorization
  - Map over number[] → SIMD
  - Reduce over number[] → SIMD horizontal ops
  - Filter → masked operations

Loop Fusion
  - arr.map(f).map(g) → arr.map(x => g(f(x)))
  - Single pass instead of two

Slice Views
  - arr.slice(a, b) returns view when possible
  - No copy for read-only use
```

### Control Flow Optimizations

```
Dead Code Elimination
  - Unreachable code removed
  - Unused computations removed
  - Constant conditionals → branch pruned

Constant Propagation
  - Track constant values through program
  - Replace variable reads with constants

Constant Folding
  - Evaluate constant expressions at compile time
  - 1 + 2 → 3, "a" + "b" → "ab"

Copy Propagation
  - a = b; use(a) → use(b)
  - Enables further optimization

Common Subexpression Elimination (CSE)
  - Compute once, reuse result
  - Must respect effects (can't reuse if side effects between)

Loop Invariant Code Motion
  - Move unchanging computations out of loop
  - Requires effect analysis

Loop Unrolling
  - Known bounds → full unroll
  - Unknown → partial unroll (4x, 8x)

Loop Unswitching
  - if inside loop with loop-invariant condition
  - → two loops, each with one branch

Branch Prediction Hints
  - likely/unlikely branches
  - Layout hot path for fall-through
```

### Function Call Optimizations

```
Inlining
  - Small functions always inlined
  - Type-specialized inlining
  - Recursive partial inlining

Tail Call Optimization
  - Tail recursive → loop
  - Tail call → jump (no stack growth)

Argument Passing
  - Small structs by value (in registers)
  - Large structs by reference
  - Multiple return values in registers

Calling Convention
  - Custom calling convention for internal functions
  - Maximize register usage
  - Minimize stack traffic
```

### Memory Optimizations

```
Load Elimination
  - Repeated loads of same field → load once
  - Requires alias analysis

Store Elimination
  - Dead stores removed
  - Store followed by overwriting store → keep last only

Load/Store Reordering
  - Move loads earlier (hide latency)
  - Move stores later (enable elimination)
  - Requires alias analysis

Prefetching
  - Insert prefetch instructions for predictable access patterns
  - Array traversals, linked list walks

Cache-Friendly Layout
  - Pack related fields together
  - Align for cache line boundaries
```

### Async Optimizations

```
Async Elision
  - Async function that never awaits → sync
  - Await on sync value → eliminate

State Machine Minimization
  - Minimize states in async state machine
  - Merge states when possible

Promise Fusion
  - Promise.all with small count → inline
  - Sequential awaits that could parallel → suggest or parallelize
```

### Parallelization

**Philosophy: Only parallelize when clear wins. Most micro-parallelization is a loss.**

```
Overhead Reality Check:
  - Thread spawn: ~10,000-100,000 cycles
  - Task queue enqueue/dequeue: ~1,000 cycles
  - Cache line transfer: ~100 cycles
  - Context switch: ~10,000+ cycles
  
  Simple addition: ~1 cycle
  
  Parallelizing two additions? 10,000x overhead.

Clear Win Criteria:
  1. Explicit signal (Promise.all, user annotation)
  2. Substantial work per task (>100μs estimated)
  3. No shared mutable state
  4. I/O bound (waiting anyway)

Never Parallelize:
  - Arithmetic operations
  - Simple field access
  - Small loop bodies (<100 iterations of simple work)
  - Sequential dependencies

Promise.all Pattern:
  // User TOLD US these are independent
  await Promise.all([
    fetch(url1),
    fetch(url2),
    fetch(url3)
  ])
  
  // Each fetch is I/O (we wait anyway)
  // User explicitly said "all at once"
  // Clear win: parallelize

Map Over Large Array:
  // Only if: body is substantial AND pure
  largeArray.map(item => expensiveComputation(item))
  
  // Cost model estimates:
  //   - expensiveComputation: ~10,000 cycles
  //   - largeArray.length: 10,000
  //   - Total work: 100M cycles
  //   - Parallel overhead: ~100,000 cycles
  //   - Benefit: 4x speedup on 4 cores = 75M cycles saved
  // Clear win: parallelize

Sequential Awaits (NO auto-parallel):
  // User wrote sequential. Maybe they meant it.
  const a = await fetch(url1)
  const b = await fetch(url2)  // Might depend on a implicitly
  
  // Don't auto-parallelize. User can use Promise.all if they want.
  // Unless we can PROVE independence and user opts in.

Cost Model:
  estimated_work(task) = 
    instruction_count * avg_cycles_per_instruction
    + memory_ops * cache_miss_probability * cache_miss_cost
    + io_ops * io_latency
  
  parallelization_benefit =
    estimated_work(all_tasks) * (1 - 1/num_cores)
    - spawn_overhead * num_tasks
    - synchronization_overhead
  
  parallelize_if: parallelization_benefit > threshold

Zero-Copy Transfer:
  - Ownership transfer when sender is done
  - No cloning for message passing
  - Requires proving non-use after send
```

### GC Optimizations

```
Write Barrier Elimination
  - Young → young: no barrier needed
  - NoEscape: no barrier needed
  - Field type can't hold old → young ref: no barrier

Allocation Sinking
  - Move allocation closer to use
  - Enables elimination if branch not taken

Pretenuring
  - Known long-lived allocations go directly to old gen
  - Avoids nursery collection + promotion cost

GC Safepoint Reduction
  - Fewer safepoints = less overhead
  - Only needed in loops and at calls
  - Can skip in provably short bounded code
```

---

## Implementation Order

### Milestone 1: Proof of Concept

```
Goal: Compile simple numeric code to native, demonstrate value proposition

Build on existing:
  - parse-js (parser) ✓ exists
  - hir-js (HIR) ✓ exists, extend with high-level ops
  - typecheck-ts (types) ✓ exists
  - optimize-js (dataflow) ✓ exists, extend with effect analysis

New components:
  - native-js/ - LLVM IR generation
  - runtime-native/ - minimal allocator

Scope:
  - Wire parse-js → hir-js → typecheck-ts → native-js → LLVM
  - Basic type flow (use typecheck-ts)
  - Basic inlining
  - LLVM codegen (no GC)
  - Simple runtime (bump allocator only)

Deliverable:
  - Compile numeric benchmark (ray tracer, matrix mult)
  - Show competitive with V8 for cold code
  - No GC (leak memory, that's fine for benchmark)
```

### Milestone 2: Complete Language

```
Goal: Support full TypeScript language features

Scope:
  - Full HIR coverage
  - Classes, interfaces, generics
  - Closures
  - Exceptions
  - Async/await

Deliverable:
  - Compile real TS code
  - Tests pass
  - Feature complete (though not optimized)
```

### Milestone 3: GC Integration

```
Goal: Proper memory management

Scope:
  - Generational GC implementation
  - LLVM statepoint integration
  - Write barriers
  - Stack maps

Deliverable:
  - Long-running programs don't crash/leak
  - GC pauses measured and acceptable
```

### Milestone 4: Analysis Engine

```
Goal: Full analysis suite

Scope:
  - All analyses implemented
  - Whole-program analysis
  - Parallel analysis where possible
  - Incremental updates

Deliverable:
  - Analysis results feed into optimization
  - Measurable improvement from analysis-driven opts
```

### Milestone 5: Optimization Suite

```
Goal: All major optimizations

Scope:
  - Everything in optimization catalog
  - Tuned heuristics
  - Performance testing infrastructure

Deliverable:
  - Competitive with V8 on all benchmarks
  - Beat V8 on cold code, predictability
```

### Milestone 6: Production Ready

```
Goal: Usable for real projects

Scope:
  - npm interop
  - Debugging experience
  - IDE integration
  - Documentation
  - Stability, testing

Deliverable:
  - Can compile real applications
  - Acceptable developer experience
  - Ready for early adopters
```

---

## Concrete Implementation Work

Organized by crate, what needs to happen:

### hir-js/ (extend)

```rust
// Add to hir.rs ExprKind enum:

/// High-level semantic operations (don't lower to loops)
ArrayMap { array: ExprId, callback: ExprId },
ArrayFilter { array: ExprId, callback: ExprId },
ArrayReduce { array: ExprId, callback: ExprId, init: Option<ExprId> },
ArrayFind { array: ExprId, callback: ExprId },
ArrayEvery { array: ExprId, callback: ExprId },
ArraySome { array: ExprId, callback: ExprId },
ArrayChain { array: ExprId, ops: Vec<ArrayChainOp> },

/// Async operations (don't lower to state machines yet)
PromiseAll { promises: Vec<ExprId> },
PromiseRace { promises: Vec<ExprId> },
AwaitExpr { value: ExprId, known_resolved: bool },

/// Known API calls (carry semantic info)
KnownApiCall { api: ApiId, args: Vec<ExprId> },

// Add ArrayChainOp:
enum ArrayChainOp {
  Map(ExprId),      // callback
  Filter(ExprId),   // predicate
  Reduce(ExprId, Option<ExprId>),  // callback, init
  Find(ExprId),
  // etc.
}

// Modify lower.rs to recognize patterns:
// - arr.map(f) → ArrayMap
// - arr.map(f).filter(g) → ArrayChain
// - Promise.all([...]) → PromiseAll
// - await expr → AwaitExpr
```

### optimize-js/ (extend analysis/)

```rust
// New file: analysis/effect.rs

pub struct EffectSet {
  pub reads: HashSet<Location>,
  pub writes: HashSet<Location>,
  pub allocates: bool,
  pub throws: ThrowBehavior,
  pub io: bool,
}

pub enum Location {
  Local(VarId),
  Field(ShapeId, FieldName),
  ArrayElement,
  Global(GlobalId),
  Heap,  // Conservative
}

pub enum ThrowBehavior { Never, Maybe, Always }

impl DataFlowAnalysis for EffectAnalysis {
  type State = EffectSet;
  // ... implement transfer functions
}

// New file: analysis/purity.rs

pub enum Purity {
  Pure,       // No effects
  ReadOnly,   // Reads but no writes
  Allocating, // Pure except allocation
  Impure,     // Has effects
}

pub fn analyze_purity(func: &Function, effects: &EffectAnalysis) -> Purity;

// New file: analysis/escape.rs

pub enum EscapeState {
  NoEscape,           // Stack allocate or scalar replace
  ArgEscape(usize),   // Escapes through argument
  ReturnEscape,       // Escapes through return
  GlobalEscape,       // Must heap allocate
}

impl DataFlowAnalysis for EscapeAnalysis {
  type State = HashMap<AllocId, EscapeState>;
  // ... implement
}

// New file: analysis/ownership.rs

pub enum Ownership {
  Owned,      // This value owns the data
  Borrowed,   // Temporary reference
  Shared,     // Multiple owners (needs GC)
  Consumed,   // Ownership transferred away
}

pub fn infer_ownership(body: &Body, escapes: &EscapeAnalysis) -> OwnershipMap;

// New file: analysis/encoding.rs

pub enum StringEncoding {
  Ascii,    // All bytes < 128
  Latin1,   // All bytes < 256  
  Utf8,     // General
  Unknown,  // Conservative
}

pub fn analyze_string_encoding(expr: ExprId, ctx: &AnalysisCtx) -> StringEncoding;
```

### NEW: effect-js/ (new crate)

```rust
// Semantic knowledge base

pub struct ApiDatabase {
  apis: HashMap<ApiId, ApiSemantics>,
}

pub struct ApiSemantics {
  pub name: String,  // e.g., "Array.prototype.map"
  pub effects: EffectTemplate,
  pub purity: PurityTemplate,
  pub properties: Vec<ApiProperty>,
}

pub enum EffectTemplate {
  Pure,
  DependsOnCallback,
  IO,
  Custom(EffectSet),
}

pub enum ApiProperty {
  Parallelizable { condition: ParallelCondition },
  Fusable { with: Vec<ApiId> },
  OutputLength { relation: LengthRelation },
  PreservesEncoding { condition: EncodingCondition },
}

// Pattern recognition
pub fn recognize_patterns(body: &Body) -> Vec<RecognizedPattern>;

pub enum RecognizedPattern {
  MapFilterReduce { array: ExprId, ops: Vec<ArrayOp> },
  PromiseAllFetch { urls: ExprId },
  TypedJsonParse { input: ExprId, target: TypeId },
  // etc.
}
```

### NEW: native-js/ (new crate)

```rust
// LLVM IR generation

use inkwell::context::Context;
use inkwell::module::Module;
use inkwell::builder::Builder;

pub struct CodeGen<'ctx> {
  context: &'ctx Context,
  module: Module<'ctx>,
  builder: Builder<'ctx>,
  
  // Type mappings
  types: TypeMap<'ctx>,
  
  // Function being compiled
  current_fn: Option<FunctionValue<'ctx>>,
  
  // Analysis results
  effects: &'ctx EffectAnalysis,
  ownership: &'ctx OwnershipMap,
  escapes: &'ctx EscapeAnalysis,
}

impl<'ctx> CodeGen<'ctx> {
  pub fn compile_function(&mut self, func: &Function) -> Result<(), CodeGenError>;
  
  fn compile_expr(&mut self, expr: ExprId) -> BasicValueEnum<'ctx>;
  
  // High-level ops compile to optimized forms
  fn compile_array_map(&mut self, array: ExprId, callback: ExprId) -> BasicValueEnum<'ctx> {
    let purity = self.effects.purity_of(callback);
    let array_len = self.get_array_length(array);
    
    if purity == Purity::Pure && array_len > PARALLEL_THRESHOLD {
      self.compile_parallel_map(array, callback)
    } else {
      self.compile_sequential_map(array, callback)
    }
  }
  
  fn compile_array_chain(&mut self, array: ExprId, ops: &[ArrayChainOp]) -> BasicValueEnum<'ctx> {
    // Fuse all ops into single loop
    self.compile_fused_pipeline(array, ops)
  }
  
  fn compile_promise_all(&mut self, promises: &[ExprId]) -> BasicValueEnum<'ctx> {
    // Actually parallel execution
    self.compile_parallel_await(promises)
  }
}

// GC integration
fn insert_gc_safepoint(&mut self);
fn insert_write_barrier(&mut self, obj: PointerValue, field: PointerValue);
fn compile_allocation(&mut self, shape: ShapeId) -> PointerValue<'ctx>;
```

### NEW: runtime-native/ (new crate)

```rust
// Native runtime library (compiled separately, linked with generated code)

// Memory
pub fn rt_alloc(size: usize, shape: ShapeId) -> *mut u8;
pub fn rt_alloc_array(len: usize, elem_size: usize) -> *mut u8;

// GC
pub fn rt_gc_safepoint();
pub fn rt_write_barrier(obj: *mut u8, field: *mut u8);
pub fn rt_gc_collect();

// Strings
pub fn rt_string_concat(a: *const u8, a_len: usize, b: *const u8, b_len: usize) -> StringRef;
pub fn rt_string_intern(s: *const u8, len: usize) -> InternedId;

// Parallel
pub fn rt_parallel_spawn(task: fn(*mut u8), data: *mut u8) -> TaskId;
pub fn rt_parallel_join(tasks: *const TaskId, count: usize);
pub fn rt_parallel_for(
  start: usize,
  end: usize,
  body: extern "C" fn(usize, *mut u8),
  data: *mut u8,
);

// Async
// NOTE: GC is moving/compacting (Immix + opportunistic copying).
// The async runtime must store something stable in OS/userdata (epoll/kqueue) and
// cross-thread wakers across awaits, so it cannot retain raw `*mut Coroutine`.
// Use a stable generational handle id (u64) that indexes a pinned handle table
// cell, and the GC updates the cell's pointer when the coroutine relocates.
pub fn rt_async_spawn(coro: CoroutineId /* = HandleId(u64) */) -> PromiseRef;
pub fn rt_async_poll() -> bool;
```

See [`docs/runtime-native/buffers-and-io.md`](docs/runtime-native/buffers-and-io.md) for the
non-moving backing store + pinning invariants required to make async I/O safe under a moving GC.

### vm-js/ and emit-js/ (unchanged)

```
vm-js/ and emit-js/ are used by other projects in the workspace.
We don't modify them for native compilation.

vm-js/ useful to us as:
  1. Reference implementation for spec behavior
  2. Test oracle (run JS, compare results with native output)
  3. Builtins as reference for native stdlib

emit-js/ not used by native compilation pipeline.
```

---

## Research Summary

### Techniques We're Adopting

From **V8**:
- Shape/hidden class concept (we compute statically)
- Packed array optimization (PackedF64, etc.)
- Inline property access (offset loads)

From **PyPy**:
- Storage strategies for collections
- Aggressive escape analysis
- Trace-based optimization thinking (we do whole-program instead)

From **GraalVM/Truffle**:
- Partial evaluation concept (specialize on constants)
- AST interpreter → native compilation pattern
- Polyglot boundaries thinking

From **Java HotSpot**:
- Scalar replacement of aggregates
- Lock elision for uncontended/unescaped
- Tiered compilation concept (dev fast, release optimized)

From **AssemblyScript**:
- Strict TypeScript subset model
- Explicit rejection of `any`
- Compile-time type enforcement

From **Immix GC**:
- Mark-region collection
- Bump-pointer allocation
- Opportunistic copying
- Line-based reclamation

From **LLVM ecosystem**:
- Statepoints for GC
- Polly for polyhedral loop optimization
- Battle-tested codegen

### Novel Combinations

What we're doing that's relatively unique:

1. **TypeScript types as ground truth** — not gradual, not optional, enforced
2. **Whole-program effect inference** — not annotations, computed
3. **Ownership inference** — Rust-like without annotations
4. **High-level IR preservation** — don't lower map/filter/Promise.all early
5. **Semantic knowledge base** — know what fs.writeFile means
6. **Streaming inference** — detect and fuse pipelines automatically
7. **SOA transformation** — array-of-structs to struct-of-arrays when beneficial

### Open Questions from Research

```
Q: Should we support some form of gradual/escape hatch?
A: Yes, for npm interop. Boundary has runtime checks.

Q: What about code that legitimately needs `any`?
A: Provide `unknown` + type guards. Force narrowing.

Q: How strict is too strict?
A: Start strict, relax based on real-world feedback.

Q: Polly integration — worth the complexity?
A: For numeric code, yes. Optional pass for hot loops.

Q: Immix vs simpler generational?
A: Immix is better for allocation-heavy JS patterns. Worth it.

Q: Conservative GC as fallback?
A: No. Precise only. Conservative would undermine optimization.
```

---

## Honest Assessment

Being rigorous and critical about this approach.

### What This Gets Right

**Preserving high-level semantics works.** V8 and other JITs lower early and rely on speculation + deopt. We can do better by not throwing away information. When we see `.map().filter().reduce()`, keeping it as a chain lets us fuse. Lowering to three loops immediately loses that.

**Promise.all is a real signal.** The user explicitly said "these are independent." JS spec says execute in order, but observe in any order. We can actually parallelize because the user told us to.

**Domain knowledge pays off.** Knowing that `Math.floor` always returns an integer, that `String.toLowerCase` preserves length for ASCII, that `fetch` is I/O—these enable real optimizations. This is what optimizing compilers do with intrinsics, just at a larger scale.

**String encoding optimization is real.** Most strings in practice are ASCII. URLs, identifiers, JSON keys, error messages. If we track encoding, we can use 1-byte representation, SIMD operations, fast comparison.

**TypeScript types are better than nothing.** Not perfect, not sound, but they tell us a lot. With whole-program analysis, we can often prove more than the type system claims.

### What's Hard

**Annotation burden is massive.** Who annotates all of Node.js? All of npm? Even if we start with core APIs, ecosystem coverage is a multi-year effort. Community contribution helps but needs critical mass to be useful.

**Correctness of semantic models.** If we model `fs.writeFile` wrong, we break programs. Our semantic database must be:
- Correct (matches actual behavior)
- Complete enough (covers edge cases)
- Versioned (APIs change between Node versions)
- Testable (we need to verify our models)

**Analysis precision vs speed.** Precise analysis (context-sensitive, field-sensitive, flow-sensitive) is slow. Imprecise analysis is fast but misses optimizations. Finding the right tradeoff is hard.

**TypeScript's unsoundness bites.** `any`, type assertions, unsound variance—these mean we can't fully trust types. We insert guards at boundaries, but that adds overhead.

**Real code is messy.** Benchmarks are clean. Production code has:
- `any` sprinkled everywhere
- Dynamic property access
- Callbacks from external libraries
- Complex control flow

### What Could Go Wrong

**Parallelization overhead.** Even with cost model, we might misjudge. Parallelizing something that takes 100μs with 50μs of spawn overhead is a loss. Need real-world calibration.

**GC pauses in production.** Generational GC is well-understood, but tuning for JS allocation patterns (lots of short-lived objects) takes work. LLVM statepoints work but are complex.

**Ecosystem compatibility.** npm packages expect V8 behavior. If we deviate (even in allowed ways), things might break subtly. Testing against real packages is essential.

**Compilation time.** Whole-program analysis is slow. For a 100k LOC codebase, how long? Minutes? Need aggressive incrementality and caching.

### What We're Betting On

1. **Well-typed TypeScript is common enough.** If most code is `any`-heavy, we lose. But modern TS codebases tend to be well-typed.

2. **High-level patterns are recognizable.** If code is obfuscated or uses weird patterns, we can't optimize it. But idiomatic code should be optimizable.

3. **Core + popular packages cover most use.** If we annotate Node core + top 100 npm packages, maybe that covers 80% of API usage.

4. **LLVM handles the low-level stuff.** We're betting LLVM's backend optimizations are good enough. If we need custom codegen for JS patterns, that's a lot more work.

### Scope Reality Check

This is a **multi-year, large-team project**. Rough estimates:

```
Component                    Effort (engineer-months)
─────────────────────────────────────────────────────
Parser integration           2
HIR design + lowering        6
Analysis engine              18
Optimization passes          24
Semantic knowledge base      12 (ongoing)
GC implementation            12
LLVM integration             6
Runtime (stdlib, async)      12
npm interop                  6
Tooling (CLI, IDE, debug)    12
Testing + hardening          24
─────────────────────────────────────────────────────
Total                        ~130 engineer-months (10+ engineer-years)
```

With "hundreds of parallel intelligent agents," this is achievable. But coordination, testing, and integration become the bottleneck.

### Why It Might Still Work

**V8 is a JIT. We're AOT.** Different tradeoffs. V8 optimizes for "run any JS fast." We optimize for "run well-typed TypeScript very fast." Narrower goal, more achievable.

**We see everything.** Whole-program analysis is our superpower. V8 never knows if some `eval()` will invalidate assumptions. We can ban `eval()` and actually prove things.

**Startup and predictability matter.** Even if we only match V8's peak performance, zero warmup and no deopt pauses is a real win for servers, CLI tools, serverless.

**The ecosystem is moving our way.** TypeScript adoption is increasing. Type coverage in npm packages is improving. The world is getting more typed.

---

## Risks and Mitigations

```
Risk: Analysis doesn't scale to large codebases
Mitigation: Incremental analysis, parallelization, approximation for cold code

Risk: GC integration with LLVM is problematic
Mitigation: LLVM statepoints are used in production (Julia), fallback to conservative GC

Risk: TypeScript types aren't strong enough for optimization
Mitigation: Require stricter subset, provide escape hatches, gradual adoption

Risk: npm ecosystem compatibility
Mitigation: Typed FFI boundary, bundle common packages

Risk: Compilation too slow
Mitigation: Incremental compilation, Cranelift for dev, tiered (fast tier for edit-compile-run)

Risk: Semantic knowledge base too expensive to build
Mitigation: Start with core, community contributions, automated extraction where possible

Risk: Parallelization overhead exceeds benefit
Mitigation: Conservative cost model, explicit opt-in, runtime measurement
```

---

## Success Metrics

```
Performance:
  - Cold code: 10x faster than V8 tier 1
  - Hot code: competitive with V8 tier 2
  - Memory: 50% less than V8
  - Startup: instant (native binary)
  - Latency: predictable (no deopts, minimal GC pauses)

Compatibility:
  - TypeScript language: 95%+ coverage
  - npm packages: typed packages work
  - Node APIs: subset implemented

Developer Experience:
  - Compile time: < 10s for incremental, < 5min for full rebuild (large project)
  - Error messages: helpful, actionable
  - Debugging: source maps, variable inspection work
```

---

## Appendix: Specific Optimization Examples

### Example 1: Vector Math

```typescript
// Input
interface Vec3 { x: number; y: number; z: number; }

function dot(a: Vec3, b: Vec3): number {
  return a.x * b.x + a.y * b.y + a.z * b.z;
}

function normalize(v: Vec3): Vec3 {
  const len = Math.sqrt(dot(v, v));
  return { x: v.x / len, y: v.y / len, z: v.z / len };
}
```

```
// After optimization

// dot inlined everywhere
// Vec3 scalar-replaced to (x, y, z) registers
// All f64 operations, no boxing
// No allocation in normalize (scalar replacement)

// Native code (conceptually):
double dot(double ax, double ay, double az, double bx, double by, double bz) {
  return ax*bx + ay*by + az*bz;
}

void normalize(double vx, double vy, double vz, double* ox, double* oy, double* oz) {
  double len = sqrt(vx*vx + vy*vy + vz*vz);
  *ox = vx / len;
  *oy = vy / len;
  *oz = vz / len;
}

// Or even better, inlined at call site with SIMD
```

### Example 2: Array Processing

```typescript
// Input
function sumSquares(arr: number[]): number {
  return arr.map(x => x * x).reduce((a, b) => a + b, 0);
}
```

```
// Analysis:
// - arr is number[] (could be PackedF64)
// - map creates intermediate array (can we eliminate?)
// - reduce is associative (can vectorize)

// After optimization:

// 1. Loop fusion: combine map and reduce
// 2. Vectorization: process 4 elements at a time
// 3. No intermediate array

double sumSquares(f64* arr, size_t len) {
  f64x4 sum = f64x4_zero();
  size_t i = 0;
  
  // Vectorized loop
  for (; i + 4 <= len; i += 4) {
    f64x4 v = f64x4_load(arr + i);
    sum = f64x4_add(sum, f64x4_mul(v, v));
  }
  
  double result = f64x4_horizontal_sum(sum);
  
  // Scalar remainder
  for (; i < len; i++) {
    result += arr[i] * arr[i];
  }
  
  return result;
}
```

### Example 3: String Key Lookup

```typescript
// Input
interface Config {
  apiUrl: string;
  timeout: number;
  retries: number;
}

function getTimeout(config: Config): number {
  return config.timeout;
}
```

```
// Analysis:
// - Config has known shape
// - "timeout" is interned property name
// - Field access is direct offset

// After optimization:

// Config layout:
// offset 0: header (8 bytes)
// offset 8: apiUrl (16 bytes, string pointer + length)
// offset 24: timeout (8 bytes, f64)
// offset 32: retries (8 bytes, f64)

double getTimeout(Config* config) {
  return *(double*)(((char*)config) + 24);
}

// One load instruction, no property lookup
```

### Example 4: Polymorphic Code

```typescript
// Input
interface Drawable {
  draw(): void;
}

class Circle implements Drawable {
  constructor(public radius: number) {}
  draw() { console.log(`Circle r=${this.radius}`); }
}

class Square implements Drawable {
  constructor(public side: number) {}
  draw() { console.log(`Square s=${this.side}`); }
}

function render(items: Drawable[]) {
  for (const item of items) {
    item.draw();
  }
}
```

```
// Analysis:
// - Only two implementations of Drawable
// - draw() is small, can inline
// - Array is homogeneous? Unknown, need runtime check

// After optimization (guarded devirtualization + inlining):

void render(Drawable** items, size_t len) {
  for (size_t i = 0; i < len; i++) {
    Drawable* item = items[i];
    if (item->vtable == &Circle_vtable) {
      Circle* c = (Circle*)item;
      printf("Circle r=%f\n", c->radius);
    } else {
      Square* s = (Square*)item;
      printf("Square s=%f\n", s->side);
    }
  }
}

// No indirect call, draw() inlined
// Type check is just pointer comparison
```
