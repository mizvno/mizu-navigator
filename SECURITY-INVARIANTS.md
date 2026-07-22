# Mizu Security Invariants

This document enumerates the capability and flow invariants for `mizu-navigator`.
It is the **single index** of security properties: each invariant is stated,
motivated, linked to the source constructs it constrains, and tagged with its
enforcement mechanism.

> **Audience**: implementers and auditors.  This spec is the input to any
> future mechanized proof effort (Kani / Creusot); see §8.

---

## 1. Navigation Invariants

**N1 — No escalation:** A network operation whose purpose is data or media
(`Fetch`, `FetchImage`, `NetworkRequest`) must never cause document navigation,
under any server response.
*Source constructs:* network worker redirect handling.
*Enforcement:* **Runtime** — callers of data/media paths never invoke
`check_navigation`; redirect loops are followed internally with a budget
(`src/network/worker.rs`).

**N2 — Single choke point:** Every top-level navigation passes through one
policy function (`check_navigation`) before any state change or
`NetworkCmd::Navigate` is emitted.
*Source constructs:* address bar, link click, `navigate` action, server redirect.
*Enforcement:* **Runtime** — `src/render/navigation.rs`, called from
`src/render/window.rs`.

**N3 — Agency:** Same-origin top-level navigation is always allowed.
Cross-origin top-level navigation is allowed only when the initiating cause
carries a user gesture.  Logic-initiated navigation without a gesture may not
leave the origin.
*Source constructs:* `NavigationInitiator` variants.
*Enforcement:* **Runtime** — navigation choke point (`check_navigation`).

**N4 — Scheme:** Only `mizu://` is navigable over the network; `file://` only
under the sandbox rules; `http(s)://` and everything else is refused at the
choke point.
*Source constructs:* `NavigationVerdict::Block`.
*Enforcement:* **Runtime** — navigation choke point.

**N5 — Uniform lifecycle:** Origin-scoped state (`capability_policy`,
redirect-chain budget, `url_registry`) is handled identically on every
navigation path.
*Source constructs:* `CapabilityPolicy` reset in `window.rs`.
*Enforcement:* **Runtime** — navigation choke point.

---

## 2. Layout Invariants

**L1 — No unmetered work proportional to remote data:** Any subsystem that
performs O(data) allocation or CPU work must draw from an explicit, named
budget.  Specifically, layout expansion (`each` nodes) is bounded by
`MAX_SYNTHETIC_LAYOUT_NODES` (20,000).
*Source constructs:* `each` iteration in `src/render/layout_bridge.rs`; nested
`each` rejection in `src/parser/layout.rs`.
*Enforcement:* **Runtime** (`layout_bridge.rs` budget counter) and
**Parse-time** (nested `each` rejection).

---

## 3. Evaluation Invariants

**E1 — Bounded evaluator recursion:** No `StateMachine::evaluate` call chain
may recurse deeply enough to overflow the native stack. `eval_depth` is a
single counter on `StateMachine`, incremented on entry to `evaluate` and
checked against `MAX_EVAL_DEPTH` (256) before any further recursion; it is
**not** reset at function-call boundaries (only `local_stack` is truncated
there), so the guard composes correctly across cross-function calls — a
~250-level function body invoked from a ~20-level-deep call-site expression
trips the guard even though neither tree alone violates
`parser::logic::MAX_PARSE_DEPTH` (256) at parse time.

*Rationale:* `MAX_PARSE_DEPTH` bounds nesting **per expression tree parsed in
isolation**. A function body is one such tree and a call-site expression is
another; nothing at parse time prevents their composition from exceeding
`MAX_EVAL_DEPTH` at evaluation time. Without a runtime counter that survives
call boundaries, a crafted (or accidental) composition could reach a native
stack overflow, which aborts the process and cannot be caught — the opposite
of the fail-secure, bounded-error behavior every other budget in this
document provides.

*Source constructs:* `eval_depth` field and guard in `src/core/types.rs`
(`MAX_EVAL_DEPTH` constant; the check in `evaluate_impl`).

*Enforcement:* **Runtime**, with a stack-size margin as defense in depth:
the guard itself is a plain integer check, but it only fires *after* one more
nested call is already on the stack, so `LogicWorker::spawn`
(`src/parser/logic_worker.rs`) runs the evaluator on a thread with an
explicit 16 MiB stack (`LogicWorker::STACK_SIZE_BYTES`) rather than the
platform default (~1 MiB on Windows, ~2–8 MiB on Linux/macOS). This size was
chosen empirically, not guessed: `core::types::tests::
measure_stack_usage_at_max_eval_depth` re-execs the test binary across a
doubling ladder of candidate stack sizes to find the smallest that survives a
300-level `evaluate()` chain (past the 256-deep guard), giving floors of
4 MiB (debug) / 256 KiB (release); 16 MiB is a ~4x/~64x margin over those
floors. `core::types::tests::cross_function_composition_depth_guard`
verifies the composed-recursion scenario specifically: it re-execs the test
binary and runs the scenario on a thread built with the *same*
`LogicWorker::STACK_SIZE_BYTES` constant production uses (not a duplicated
literal), so the test tracks production's actual stack size if that constant
ever changes.

---

## 4. Storage Invariants

**S1 — Write-only from the document side:** The document may remember, but
what it remembers never leaves the device.  Storage is write-only from the
document's side; no logic-reachable path returns stored values into the
expression evaluator.
*Source constructs:* `store_local` builtin in `src/core/types.rs`.
*Enforcement:* **Code boundary contract** — no `read_local` primitive is
exposed.  *Note: if `read_local` is ever added, it must be declared as a taint
source in invariant F1 and route through the load-time flow checker.*

**S2 — Debounced writes are eventually durable, not immediately durable
(RM-12):** `NetworkCmd::StorageStore` commands for the same origin are
batched via `StorageWriteDebouncer` (`src/network/worker.rs`) into a single
`redb` transaction per debounce window (`STORAGE_DEBOUNCE_WINDOW`, 150ms) or
per `STORAGE_BATCH_MAX_KEYS` (64) buffered keys, whichever comes first,
instead of one transaction per `store_local` call. A write is durable only
once its batch commits — a crash, `kill -9`, or power loss within the window
can lose the most recent writes to an origin even though `store_local`
already returned. This is an accepted tradeoff, not a gap: **S1** already
guarantees a document can never observe whether a given write has landed (no
`read_local`), so no document-visible invariant is weakened. It does **not**
apply to authentication tokens/credentials: `VaultEntry` (`src/network/vault.rs`)
writes straight to the OS keyring on every `save()` call and never goes
through `StoragePool`/`redb` at all, so bearer tokens keep an unconditional
immediate-write guarantee.
*Source constructs:* `StorageWriteDebouncer::submit` in `src/network/worker.rs`;
`StorageEngine::write_batch` in `src/core/storage.rs` (the still-available,
non-debounced `StoragePool::write_record` bypasses this entirely).
*Enforcement:* **Design boundary** — the debounce window is short and bounded
by both time and key count; no document-observable correctness property
depends on write timing (see S1).

---

## 5. Purity Invariants

**P1 — Purity in observation contexts:** A class condition (`class X if
<expr>`) and any future pure-context expression must contain no effectful
construct.  An *effectful construct* is any `Expr::FunctionCall` whose name
resolves to a construct that is *not* in the set of user-defined functions
*and* not in the known-pure builtins allowlist.

*Rationale:* Class conditions are re-evaluated on every frame; they are
observation points, not action points.  Side effects in an observation context
would break referential transparency and allow network or storage activity
proportional to the rendering frame rate.

*Source constructs:* `Expr::FunctionCall` in conditional-class condition
expressions (`src/parser/layout.rs`).

*Enforcement:* **Parse-time** — `find_effectful_call` in
`src/parser/logic.rs`.  Uses a **pure-builtins allowlist** (not a denylist):
any function call whose name is not a user-defined function and not a
known-pure builtin is conservatively treated as effectful and rejected.

*Known-pure builtins:* `filter`, `count`, `sort`, `len`, `to_string`,
`contains`, `starts_with`, `ends_with`, `replace`, `concat`, `reverse`,
`json_encode`, `map`, `sort_by`, `validate_path`.

*Effectful intrinsics* (push to `accumulated_actions` in the evaluator or
produce `Action` nodes at the parser level): `GET`, `POST`, `PUT`, `DELETE`,
`QUERY`, `navigate`, `store_local`, `copy_to_clipboard`, `download`,
`get_system_time`.

*Structural justification:* These names are the evaluator's dispatch keys
(`src/core/types.rs`).  There is no AST-level structural difference between a
pure and an effectful `FunctionCall`; the name *is* the structure.  The
allowlist inverts the maintenance burden: new pure builtins must opt in; new
effectful builtins are rejected by default (**fail-secure**).

---

## 6. Information-Flow Invariants

**F1 — Gated information flow:** No value derived from an untrusted source may
reach a capability-determining sink without passing a declared, validated gate.

*Rationale:* Untrusted remote data must not silently upgrade into a device
capability (changing the application's origin or restructuring an API path)
without explicit sanitisation or user agency.

*Source constructs:* `Action::Navigate.url`, `NetworkCall.path_param`,
`NetworkCall.target_var`, `$form` fields.

*Enforcement:* **Load-time** — `check_information_flow` in
`src/parser/flow.rs`.  Runs after `check_dag` and `comp` extraction, before
the document is considered ready.

### Taint Lattice

Two-point lattice: **Clean** / **Tainted**.

| Category | Items | Notes |
|---|---|---|
| **Sources** (Untrusted) | `NetworkCall.target_var` (values bound from network responses), `$form` fields, `read_local` (specified-but-empty — see S1) | A variable assigned from a `NetworkCall` or `$form` is tainted from load. |
| **Sinks** (Capability-determining) | `Action::Navigate.url`, and any future destination-bearing expression. | — |
| **Non-sinks** (important) | `NetworkCall.alias_sym`, `NetworkCall.path_param` | The alias is a `Symbol` resolved against the static `urls` registry at parse time (`parse_action_with_urls`), so the host is enumerable by construction; the taint concern is path/params, not the alias.  `path_param` is gated by construction: every runtime evaluation validates A1 (type: string/number) and A2 (single segment, no delimiters, percent-encoded) — see `logic.rs:2122-2150`. |

### Taint Propagation

- **Expressions:** Taint propagates transparently through `Expr` nodes:
  `BinaryOp`, `Let`, `IfElse`, `FieldAccess`, `FunctionCall`, `Not`.  If any
  sub-expression is tainted, the whole expression is tainted.
- **Functions:** A function is tainted if any tainted argument reaches its
  result, or if it reads a tainted global variable (computed via the
  reachable-reads walk — `collect_reachable_function_reads`).
- **Assignments:** An `Assign` of a tainted expression taints its target
  variable.
- **Computed variables:** A `comp` variable with a tainted RHS is tainted.
  Propagation through `comp` chains is iterative (fixpoint).

### Soundness and Precision

- **Sound:** The checker never misses a real source→sink flow.  Any analysis
  uncertainty (unresolved symbol, unexpected node) is treated as
  tainted/rejected, never as clean.
- **Conservative:** Over-approximation is acceptable.  Spurious rejections are
  tolerated to keep the checker small, fast (linear in AST size), and
  single-pass.  A rejected document is a *compile error* the author can fix by
  routing through a gate, not a silent downgrade.
- **Single-pass sufficiency:** The flow graph is a DAG (enforced by
  `check_dag`).  The taint fixpoint is reached by iterating
  taint-propagation through functions and comps until stable.  Because
  the graph is acyclic and finite, convergence is guaranteed.

### Gates

Gates *discharge* taint.  Taint that reaches a sink without passing through a
gate is a compile error.

- **Navigation gesture gate (G1):** A user-gesture–triggered `navigate` action
  (`click`, `submit` event handler) discharges navigation-sink taint.  In the
  static model, actions from `EventBlock::Click` and `EventBlock::Submit` are
  considered gated; actions from `RootTimer` and network-response handlers are
  not.
- **Path parameter validation gate (G2):** The `path_param` runtime A1+A2
  validation (rejecting delimiters and percent-encoding) acts as a gate by
  construction.  Every `path_param` expression is validated at evaluation time;
  the static checker does not flag `path_param` as a sink because the gate is
  unconditional.

---

## 7. Enforcement Classification

| ID | Invariant | Enforcement | Location |
|---|---|---|---|
| N1 | No fetch→navigation escalation | Runtime | `src/network/worker.rs` |
| N2 | Single choke point | Runtime | `src/render/navigation.rs` |
| N3 | User gesture for cross-origin | Runtime | `check_navigation` |
| N4 | Scheme allowlist | Runtime | `check_navigation` |
| N5 | Uniform lifecycle | Runtime | `src/render/window.rs` |
| L1 | Layout budget | Runtime + Parse-time | `layout_bridge.rs`, `layout.rs` |
| E1 | Bounded evaluator recursion | Runtime + stack-size margin | `types.rs` (`eval_depth`), `logic_worker.rs` (`STACK_SIZE_BYTES`) |
| S1 | Write-only storage | Code boundary | `types.rs` (no `read_local`) |
| S2 | Debounced writes are eventually (not immediately) durable | Design boundary | `StorageWriteDebouncer` in `worker.rs` |
| P1 | Purity in observation contexts | Parse-time | `find_effectful_call` in `logic.rs` |
| F1 | Gated information flow | Load-time | `check_information_flow` in `flow.rs` |

---

## 8. Kani / Creusot Handoff

The following invariants are candidates for mechanized proof as a separate
assurance workstream:

1. **N2 + N3 + N4** — `check_navigation` is a pure function with no I/O.
   A Kani harness can exhaustively verify the scheme and origin checks for all
   `NavigationInitiator` variants.
2. **L1** — The layout budget counter in `layout_bridge.rs` is a simple
   monotonically-decreasing integer.  A proof can show it never exceeds
   `MAX_SYNTHETIC_LAYOUT_NODES`.
3. **F1** — The taint fixpoint in `flow.rs` terminates (the graph is a DAG and
   the tainted set only grows).  A proof can show soundness: every source→sink
   path in the AST is detected.

This document (`SECURITY-INVARIANTS.md`) serves as the specification input to
any such effort.

---

## 9. Out of Scope (documented follow-ups)

- SMT-level *value* properties (e.g. proving a specific string can't contain a
  delimiter) — taint reachability needs no solver.
- Actual `read_local` implementation (the source is specified-but-empty until
  then).
- A `links`/declared-external-destination syntax extension.
