# Mizu Language Semantics

> **Status:** July 2026.  This document describes **decided, implemented** behavior
> extracted from the authoritative sources.  Items marked ⚠ are edge cases that
> previously lived only in code.
>
> For the security invariants and the information-flow model see **`SECURITY-INVARIANTS.md`**
> (do not re-read here — cross-link only).
> For the numeric-model freeze decisions see **`SEMANTICS.md`** if it exists;
> this document cross-links and supplements that freeze.

---

## Contents

1. [Execution Model Overview](#1-execution-model-overview)
2. [Value Types](#2-value-types)
3. [Numeric Model](#3-numeric-model)
4. [Expression Evaluation](#4-expression-evaluation)
5. [Variable and State Model](#5-variable-and-state-model)
6. [Functions](#6-functions)
7. [Computed Variables](#7-computed-variables)
8. [Event / Action Semantics](#8-event--action-semantics)
9. [Timers](#9-timers)
10. [Network Calls](#10-network-calls)
11. [The `$form` Magic Record](#11-the-form-magic-record)
12. [Resource Bounds (First-Class Semantics)](#12-resource-bounds-first-class-semantics)
13. [String Interpolation](#13-string-interpolation)
14. [Layout Semantics](#14-layout-semantics)
15. [Termination Story](#15-termination-story)
16. [Capability and Flow Model](#16-capability-and-flow-model)

---

## 1. Execution Model Overview

**Source:** `src/parser/logic_worker.rs` — `LogicWorker::run_loop`

Mizu uses a **reactive, event-driven, single-threaded state machine** split across two threads:

```
UI thread                   Logic worker thread
──────────────────          ──────────────────────────────
Paint → read store          Receive UiEvent
Interpolate text            Execute action / timer / form submit
                            Mutate VariableStore
                            Recompute comp vars (topo order)
                            Send WorkerResponse (diff of changes)
UI thread ← receive         Update global store
Re-render changed nodes
```

Key properties:
- **Every reaction ends.** Each action execution is bounded by `MAX_INSTRUCTIONS` (20 000 AST node evaluations). A reaction that hits the budget returns `MizuError::Timeout` and leaves the store unchanged.
- **No self-waking.** Logic code cannot schedule its own execution; only events (click, submit, timer, network response) drive the worker.
- **Top-to-bottom read.** The document is rendered deterministically; there is no async data-binding on the UI thread.
- **Known names only.** Variable names are interned at parse time; runtime-generated names (form fields, network response targets) that were not declared at parse time are silently dropped.

---

## 2. Value Types

**Source:** `src/core/types.rs` — `Value` enum

| Mizu type name | Rust variant | Notes |
|---|---|---|
| `null` | `Value::Null` | Absence of a value; default for unset globals |
| `bool` | `Value::Bool(bool)` | `true` / `false` |
| `num` | `Value::Int(i64)` | **The only numeric variant.** A fixed-point decimal: the `i64` holds the value scaled by `DECIMAL_SCALE` (`10_000`), giving 4 decimal digits of precision. There is no `Value::Float` — see §3. |
| `string` | `Value::String(Arc<str>)` | UTF-8, reference-counted |
| `list` | `Value::List(Arc<Vec<Value>>)` | Ordered, heterogeneous, reference-counted |
| `record` | `Value::Record(Arc<[(Arc<str>, Value)]>)` | A reference-counted, **sorted slice** of key-value pairs (not a `BTreeMap`) — sorted ascending by key at construction time; field lookup is a binary search (`Value::get_field`) |

⚠ **`null` semantics:** An unset global variable evaluates as `Null`.  Reading a global bound to `Null` returns `MizuError::VariableNotFound`, not `Null` itself — this means `null` is not a programmable sentinel; it is "not yet set."

---

## 3. Numeric Model

**Source:** `src/parser/logic.rs` — `parse_expr` (literal construction), `apply_binop` (arithmetic); `src/core/types.rs` — `DECIMAL_SCALE`, `Value::Int`

### Fixed-point, not float — there is no `Value::Float`

⚠ **This section previously described an `Int`/`Float` dual-type model
(`n.fract() == 0.0 → Int`, otherwise `Float`). That model does not exist in
the current implementation** — `Value` has no `Float` variant at all
(corrected in the MNT-01 pass; see §2). Every numeric literal, regardless of
whether it has a fractional part, becomes a single `Value::Int(i64)`
holding the value scaled by `DECIMAL_SCALE = 10_000` (4 decimal digits of
precision), rounded to the nearest representable value:

- `4` → `Value::Int(40_000)`
- `4.5` → `Value::Int(45_000)`
- `0.0001` → `Value::Int(1)` (the smallest representable non-zero magnitude)

Display formatting (`Value::Display`) reverses the scaling for output, so a
document author never sees the raw scaled integer — `4` prints as `4`,
`4.5` prints as `4.5`.

### Arithmetic

All four operators dispatch on `(BinOp, Value::Int, Value::Int)` — there is
no separate float code path to select between:

- **`+`, `-`:** `checked_add`/`checked_sub` on the scaled `i64` directly (scaling is additive-invariant, so no rescale is needed). Overflow → `MizuError::ExecutionError("integer overflow")`.
- **`*`:** `checked_mul` on the two scaled values, then divided by `DECIMAL_SCALE` to undo the doubled scale. Overflow (of the pre-divide product) → `MizuError::ExecutionError("integer overflow")`.
- **`/`:** the left operand is first scaled again (`l.saturating_mul(DECIMAL_SCALE)`) so the subsequent integer division preserves precision, then `checked_div` by the (unscaled) right operand. Division by zero → `MizuError::DivisionByZero`. Overflow of the pre-divide numerator, or a `checked_div` failure (e.g. `i64::MIN / -1`) → `MizuError::ExecutionError("integer overflow")`.

⚠ Every arithmetic result is always `Value::Int` — there is no whole-number
check that selects a different output type the way the old
`Int`-if-remainder-0 model implied. `4 / 2` and `5 / 2` both produce
`Value::Int` (`20_000` and `12_500` respectively, i.e. `2` and `2.5` when
displayed) — fixed-point division does not lose the fractional part the way
truncating integer division would.

Non-numeric operands to an arithmetic operator → `MizuError::TypeError`.

### String concatenation

`String + String` is allowed via `BinOp::Add` and produces a new `String`.  Any other type with `+` → `TypeError`.

---

## 4. Expression Evaluation

**Source:** `src/core/types.rs` — `StateMachine::evaluate_impl`

### Evaluation order

Left-to-right, strict (eager) evaluation of sub-expressions, **except** for `IfElse` which is **lazy**:

- `if cond then a else b` — `cond` is evaluated first; only the selected branch (`a` or `b`) is evaluated.  The un-selected branch is never evaluated.
- `cond ? a : b` — same semantics; both are the same `IfElse` AST node.

### Variable lookup

1. Check the **local stack** (function parameters and `let` bindings).  The innermost binding whose stack index is ≥ the current frame pointer wins (lexical scoping).
2. Check the **global store** (`global_store`).  If the global is `Null`, fall through to error.
3. → `MizuError::VariableNotFound(name)`

### Instruction budget

Each `evaluate` call increments `instruction_count`.  Once `instruction_count > MAX_INSTRUCTIONS` (20 000), the call returns `MizuError::Timeout`.  The counter is reset to 0 before each top-level action or timer evaluation.

⚠ **List builtins** charge the budget proportional to the list length (`filter`, `count`, `sort`) before any iteration.  A large list can exhaust the budget before iterating a single element.

### Nesting depth

`eval_depth` is incremented on entry and decremented on exit.  Exceeding `MAX_EVAL_DEPTH` (256) → `MizuError::ExecutionError("evaluation nesting too deep (max 256 levels)")`.

---

## 5. Variable and State Model

**Source:** `src/core/types.rs` — `VariableStore`, `StateMachine`

### Global variables

Declared by `name = expr` at the root of the `logic` block (zero-parameter functions).  Stored in `StateMachine::global_store` (a `FxHashMap<Symbol, Value>`).

### Zero-argument functions as variables

`pi() : 3.14159` and `pi = 3.14159` are both valid; the latter is syntactic sugar for the former.  Both declare a zero-parameter function whose body is the expression.  The logic worker evaluates zero-argument functions once at document load and stores the result in `global_store`.

### Computed variables

See §7.

### Frozen interner (known names only)

After the parse phase the `StringInterner` is **frozen**.  Only names declared in the `logic` block are in the interned symbol table.  Runtime-generated names (form field names, network response target variables) that are not declared are dropped silently via `set_runtime` — they never appear in the store.

To make a variable reachable from a network response, declare it in the `logic` block:
```
count = 0
```
Then reference it as the target of a `GET(api) -> count` call.

### Undo log and diffing

Every global mutation via `set_global` records `(symbol, old_value)` in `undo_log`.  After an action completes, the worker diffs the undo log against the current store to produce the set of *changed* symbols.  Only changed symbols are sent to the UI thread (`WorkerResponse`).

---

## 6. Functions

**Source:** `src/parser/logic.rs` — `parse_function_block`, `StateMachine::evaluate_impl`

### Parameter passing

Arguments are evaluated left-to-right in the *caller's* scope.  Then a new frame pointer is set to the current top of the local stack.  Each parameter is pushed onto the local stack.  After the call, the local stack is truncated to the saved frame pointer.

⚠ **Outer-scope isolation:** function parameters shadow globals of the same name **within** the function body; globals are never modified by a function call.  A function reading a global variable `z` that the caller also has bound locally will see the **global** value of `z` (the frame pointer prevents the caller's local `z` from leaking into the callee).

### Type checking

If a parameter has a type annotation (`p: num`), the argument value is checked at call time via `check_type`.  Mismatch → `MizuError::TypeError`.  Unannotated parameters accept any value.

### Arity

Exact arity is required; too few or too many arguments → `MizuError::ParseError`.

### Multi-line bodies

Each non-last line must be `name = expr` (a local let-binding).  The last line must be a bare expression (the return value).  Desugared into nested `Expr::Let` nodes.

### `get_system_time` (RM-04)

`get_system_time(target)` is a built-in, not a user-defined function, but is
subject to a comparable arity/shape restriction: `target` must be a single
**bare variable identifier**, checked at parse time (`parser::logic.rs`) —
any other expression (a literal, a computed expression, a field access) is
`ParseError`. `target` additionally may not name a `comp` variable, checked
at load time by `parser::flow`. Both restrictions exist so the write
destination is always a statically-known `Symbol`, never derived from
untrusted data (before RM-04, the argument was evaluated at runtime to
select the target, giving `$form`/network-response data a channel to choose
*which* variable got overwritten). The delivered timestamp is queued as a
`RuntimeAction::GetSystemTime` effect and arrives asynchronously via the
same `UiEvent::UpdateVariable` path a network response uses — it is never
attacker-influenced, so (unlike form/network data) it needs no flow-checker
taint tracking.

---

## 7. Computed Variables

**Source:** `src/parser/logic.rs` — `parse_computed_with_functions`, `recompute_computed_bindings`

```
comp total = price * qty
```

- Declared with `comp name = expr` at the root of the `logic` block.
- **Read-only at runtime:** assigning to a `comp` variable is `ExecutionError`.
- **Topological evaluation:** the parser sorts `comp` bindings so dependencies are always computed before dependents.  A cycle among `comp` variables is `ParseError`.
- **Dependency over-approximation:** the dependency set includes all variable symbols *transitively* read by called logic functions (`collect_reachable_function_reads`).  Extra dependencies are harmless (they trigger a spurious recompute); missing dependencies would cause stale values.

⚠ After any mutation the logic worker calls `recompute_computed_bindings`.  Only comp vars whose `depends_on` set intersects the set of mutated symbols are recomputed.

---

## 8. Event / Action Semantics

**Source:** `src/parser/logic_worker.rs` — `run_loop`; `src/parser/logic.rs` — `execute_action`

Events:

| `UiEvent` | Trigger |
|---|---|
| `Click { node_id }` | User clicks a node with `click -> action` |
| `SubmitForm { submitter_node_id, fields }` | User submits a form |
| `RootTimer { index }` | A root `timer` fires |
| `UpdateVariable { name, value }` | A network response arrives |

Each event causes:
1. `undo_log.clear()`
2. Action executed via `execute_action`
3. `recompute_computed_bindings` on the set of mutated symbols
4. `send_response` — diff undo log → `WorkerResponse`

### Concurrent fetch / last-issued-wins

Network calls are issued as `RuntimeAction::NetworkCall` entries in `accumulated_actions`.  Multiple concurrent fetches to the same target variable result in **last-response-wins** behavior — the variable is updated whenever a response arrives, regardless of order.

---

## 9. Timers

**Source:** `src/parser/logic.rs` — `parse_root_timers`; `src/parser/logic_worker.rs`

- Declared as `timer <interval> -> <action>` at the root of the `logic` block.
- No node-local timers (no `every` attribute); this is permanent by design.
- Interval forms: `500ms`, `1.5s`, `1000` (bare integer, treated as ms), or a variable name.
- The minimum practical interval is enforced by the platform's timer resolution (typically 16 ms for 60 fps).
- The timer index in `UiEvent::RootTimer { index }` corresponds to the declaration order in the source.

---

## 10. Network Calls

**Source:** `src/parser/logic.rs` — `parse_action_with_urls`, `execute_action`; `src/parser/logic_worker.rs`

Network calls are validated at **parse time** against the `urls` registry:

- `GET(alias) -> var` — alias must be an `api` endpoint.
- `POST(alias, payload) -> var` — alias must be an `api` endpoint; payload evaluated to `Value` and JSON-encoded.
- `PUT(alias, payload) -> var` — same.
- `DELETE(alias) -> var` — alias must be an `api` endpoint.
- `QUERY(alias, payload) -> var` — non-standard server-side filter (alias must be `api`).
- `download(alias)` — alias must be a `media` endpoint; triggers a file download.

At execution time network calls are **queued** as `RuntimeAction::NetworkCall` in `accumulated_actions` and dispatched asynchronously by the network layer.

### `path_param` rules

Evaluated to a string or number.  The resulting string is validated as a **single path segment** — it must not contain `/`, `\`, `..`, or ASCII control characters.  Failure → `ExecutionError`.

---

## 11. The `$form` Magic Record

**Source:** `src/parser/logic_worker.rs` — `run_loop` / `SubmitForm` handler

On form submission, the logic worker:

1. Builds a `Value::Record` from all submitted fields and stores it as `$form`.
2. Updates each field name individually via `set_runtime` (respects frozen interner; undeclared fields are **dropped**).
3. Executes the `submit -> action`.

⚠ `$form.field` dot-path access works for any field included in the form, even if `field` is not declared as a standalone variable.  However, undeclared field names that are submitted via an `input name "…"` but not in the logic block are **silently dropped** by `set_runtime` and will not be settable as global variables — only `$form.fieldname` access works for them.

---

## 12. Resource Bounds (First-Class Semantics)

**Source:** `src/core/types.rs` — `MAX_INSTRUCTIONS`, `MAX_EVAL_DEPTH`, `MAX_JSON_DEPTH`, `MAX_COMP_BINDINGS`; `src/parser/logic.rs` — `MAX_PARSE_DEPTH`

These bounds are **language semantics**, not implementation details.

| Resource | Bound | Source constant |
|---|---|---|
| Instruction budget per reaction | 20 000 AST node evaluations | `MAX_INSTRUCTIONS` |
| Expression nesting depth | 256 | `MAX_EVAL_DEPTH` |
| Parse expression nesting depth | 256 | `MAX_PARSE_DEPTH` |
| JSON deserialization depth | 256 (tied to `MAX_EVAL_DEPTH`, RM-07) | `MAX_JSON_DEPTH` |
| Dot-path interpolation depth | 64 | `MAX_RECORD_DEPTH` (inline constant) |
| `comp` bindings per document | 500 (RM-08) | `MAX_COMP_BINDINGS` |

⚠ **`MAX_JSON_DEPTH` was previously documented as `64`** (independent of
`MAX_EVAL_DEPTH`) — this was corrected in RM-07: it is now defined as
`const MAX_JSON_DEPTH: u32 = MAX_EVAL_DEPTH` (256) specifically so that any
`Value` the evaluator can legally build (nested up to `MAX_EVAL_DEPTH`) is
guaranteed re-readable from storage; a lower JSON limit would silently brick
a stored value the evaluator itself was allowed to construct. Corrected in
the MNT-01 pass.

⚠ **List builtin budget cliff:** `filter(list, field, value)`, `count(list, field, value)`, and `sort(list, field, dir)` each charge `list.len()` (or `n * log2(n)` for sort) to the instruction counter **before** iterating.  A 20 001-element list with a single `filter` call will time out immediately.

Other bounds (layout node cap, redirect limit, response body cap, storage quotas) are defined in the network/layout layers and cross-linked from `SECURITY-INVARIANTS.md`.

---

## 13. String Interpolation

**Source:** `src/core/types.rs` — `StateMachine::interpolate_into_with_overlay`

- `{varname}` — replaced with `format!("{}", value)` for the named variable.
- `{a.b.c}` — dot-path resolution through nested `Record` values; if the path fails to resolve, the literal `{a.b.c}` is emitted and a `tracing::warn!` is logged.
- `\{` → `{`, `\\` → `\`, `\}` → `}` (escape sequences).
- Unmatched `{` (no closing `}` before another `{` or end-of-string) is emitted literally.
- Unknown variable names → literal `{varname}` emitted + `tracing::warn!`.

⚠ **`each` overlay:** inside an `each item in list` loop, the iteration variable `item` is resolved from an **overlay** map before the global store, allowing the current element to shadow a global of the same name.

---

## 14. Layout Semantics

**Source:** `src/parser/layout.rs`

### Tree structure

The layout tree is a single-root `ego-tree` with `MizuNode` values.  The root must be `window`.

### `each item in list`

- Renders one subtree per element of the named list variable.
- Nested `each` is `ParseError`.
- Each iteration creates an overlay binding `item → element_value`; dot-path access (`{item.field}`) works in text content and attributes via the interpolation overlay.

### Conditional classes

`class name if expr` — `expr` must be pure; evaluated on each render frame.  If `true`, the class name is appended to the node's active class set for that frame.

### Image src resolution

**Source:** `src/parser/layout.rs` — `parse_layout_with_urls`

1. If src is an absolute URL (`mizu://`, `http://`, `https://`) → `ParseError`, **unconditionally** — this check runs regardless of whether a `urls` registry was supplied.
2. The remaining steps only run when a `urls` registry is supplied (`parse_layout` — no registry — skips straight to accepting `src` as-is):
   1. If `is_remote_origin` and src starts with `file://` → `ParseError`.
   2. If src contains `.` or `/`, **or** (src starts with `file://` and the document is *not* remote-origin) → treated as a direct path and used as-is, unchanged.
   3. Otherwise → looked up as a `media` alias in the `urls` registry; missing or wrong kind → `ParseError`; found → rewritten to `raw_target`.

⚠ **Known gap (MNT-01):** step 2.2 applies **regardless of `is_remote_origin`**
— a remote-origin document's `image src` containing `.` or `/` (e.g.
`image src "assets/logo.png"`) is accepted as a direct path exactly like a
local-origin document's, with no registry lookup or rejection. This section
previously stated "if `is_remote_origin` and src is a relative path →
`ParseError`" as if that were implemented; it is not — verified empirically
(`parse_layout_with_urls(..., is_remote_origin = true)` on a relative `src`
returns `Ok`, not `Err`). Only the `file://` scheme is actually rejected for
remote origin (step 2.1). This is flagged as a suspected parser bug, not a
corrected specification — see `walkthrough.md`'s "MNT-01" entry for the full
finding; do not treat plain-relative-path rejection as enforced for
remote-origin documents until this is resolved.

---

## 15. Termination Story

Every reaction in Mizu **terminates**:

1. **Acyclic call graph** — recursion (direct or mutual) is rejected at parse time. All user-defined functions form a DAG; the call depth is statically bounded.
2. **Instruction budget** — even if the call graph were somehow cyclic (impossible after §1), `MAX_INSTRUCTIONS` (20 000) ensures the evaluator returns within microseconds.
3. **No loops** — there are no `while`, `for`, or `loop` constructs in the expression language.
4. **`comp` cycles** — rejected at parse time; `comp` recomputation always terminates.

The document *reacts* while open (timers, events), but **each reaction ends** — matching the MANIFESTO's honest wording: "reactions end, top-to-bottom read, no self-waking, known names."

---

## 16. Capability and Flow Model

See **`SECURITY-INVARIANTS.md`** for normative detail.  Summary:

- **`urls` registry** is the sole network capability surface.  Every outbound request must reference a declared `api` or `media` alias.
- **Navigation** (`navigate expr`) requests a full document replacement.  The navigator is responsible for enforcing navigation policies.
- **Storage** (`store_local(key, value)`) is write-only from the document's perspective.  No `read_local` exists; stored values cannot feed back into the evaluator or reach any network call.  **Durability note (RM-12):** writes to the same origin are debounced — closely-spaced calls are batched into a single encrypted-storage transaction on a short (~150ms) delay, or sooner if a document writes many distinct keys in a burst — instead of one disk transaction per call.  A write is not guaranteed durable until its batch commits, so data from the last debounce window can be lost if the app terminates abnormally (crash, force-kill, power loss) in that window.  This does not apply to authentication tokens, which are written immediately to the OS keyring on every call, unaffected by this debounce.
- **Information-flow:** untrusted data (network responses, form inputs) reaches the store via `set_runtime` (which respects the frozen interner and discards undeclared names).  It cannot flow into `path_param` or `navigate` without an explicit assignment action that a document author consciously writes.
- **Imports:** local-file only; network-origin documents cannot import files.
- **`file://` asset references** in remote documents are `ParseError`.
