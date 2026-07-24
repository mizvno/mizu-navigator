# Model-to-Code Correspondence and Trusted Base

This document details the fidelity ledger between the `λ_mizu` mechanized model and the shipped Rust implementation. An interactive theorem prover proof certifies only the model; this document provides an honest account of every abstraction, idealization, and divergence, bridging the gap between the proven properties and the real-world runtime. Without this ledger, the proof gives false confidence.

## Trusted Base (Out of Scope)
The following components are part of the trusted base. They are entirely unmodeled, meaning the theorems assume their correctness:
1. **Network Stack and TLS**: The networking layer, DNS, TLS termination, and the `reqwest` / HTTP stack are assumed to correctly deliver responses.
2. **Rendering and Vello**: The UI rendering pipeline, synthetic DOM generation, and the layout engine (other than its node-budget consumption).
3. **Storage-at-Rest Crypto**: The keyring and on-disk cryptographic handling.
4. **Concurrency and Tokio Scheduler**: The model abstracts the Tokio async runtime into a serialized last-issued-wins result semantics, ignoring scheduler-level concurrency and race conditions outside of that model.

## Abstractions and Idealizations

### 1. Binder Representation
`Let` and function parameters bind using **named symbols with runtime environments** instead of De Bruijn indices or locally-nameless representations.
* **Rationale**: The Rust evaluator (`StateMachine::evaluate`) performs no substitution. It resolves `Expr::Variable` by name against a local stack (innermost binding wins) and a global store. A named-environment semantics is the faithful model and entirely eliminates substitution metatheory.

### 2. The Frozen Interner
The property "no new symbols at runtime" is modeled by restricting runtime values and states to a finite set of declared `Symbol`s.
* **Correspondence**: Mirrors the Rust runtime behavior where the interner is frozen after parse (`set_runtime` drops writes to undeclared names). Actions like `Assign.target` and `NetworkCall.target_var` are modeled as pre-interned `Symbol`s instead of runtime strings.

### 3. Budgets and Execution Cost
The model explicitly instrumented evaluation with instruction budgets (`MAX_INSTRUCTIONS`) and synthetic layout node budgets (`MAX_SYNTHETIC_LAYOUT_NODES`).
* **Correspondence**: Mirrors the strict resource bounds enforced in the Rust evaluator (`instruction_budget`). The termination theorem (T1) relies entirely on this instrumentation, ensuring a data-independent resource bound. 

### 4. Values and Types
* **No Floating-Point Variant**: `Value` (`core::types::Value`) has no `Float` variant to begin with — JSON numbers (integer or floating-point alike) are converted to the fixed-point `Value::Int` (scaled by `DECIMAL_SCALE`), and non-exact division produces a runtime error rather than a float result. This is not a case of the model omitting something the Rust type has; `Val` (the Lean model's value type) has no `.float` constructor because `Value` never had one in this version of the code either.
* **Record Representation**: `Record` is modeled as an association list rather than a `BTreeMap`.

### 5. Divergences in Flow Checker Execution
The Rust `check_information_flow` (`flow.rs`) iterates `while changed` to the least fixpoint. The model instead iterates a syntactic bound (`#functions + #comps + #actions + 1`) and then explicitly checks stability.
* **Rationale**: This makes soundness independent of a convergence argument (it is fail-secure), matching the checker's philosophy while maintaining precision since the bound dominates the number of productive iterations.

### 6. Builtins and Handlers
* **`get_system_time` Included (RM-04)**: Previously excluded because its target-variable name was runtime-evaluated as a string, giving untrusted data a write-target selection channel. Fixed in the Rust parser (`parser::logic.rs`): the argument must now be a bare identifier, resolved to a `Symbol` at parse time and never evaluated — structurally identical to `download`'s alias argument. Modeled the same way, via `Builtin.getSystemTime` / `evalGetSystemTime` (`Semantics.lean`), queuing an `Effect.getSystemTime` rather than writing the store directly (expression evaluation never writes `σ` in this model). The delivered timestamp value itself is not modeled — it arrives later via the same delivery path a `NetworkCall` response uses, and (like `storeLocal`/`copyClipboard`/`download`) is out of `T2_non_interference`'s scope, since it is never attacker-influenced data.
* **`ComputedBinding` Dependency Resolution**: The static `depends_on` list is computed by the model rather than stored, reflecting what `parse_computed_with_functions` derives.
* **Handler Mapping**: The model indexes handlers (clicks, submits, timers) positionally via lists rather than by node ID.

### 7. Integer Overflow in `applyBinop` (RM-06)
`Val.int` is Lean's unbounded `Int`; the Rust runtime's `Int` is a fixed-point value backed by `i64`. `applyBinop`'s `add`/`sub`/`mul` cases never fail in the model, whereas `apply_binop` (`logic.rs`) uses checked arithmetic on all three and returns `MizuError::ExecutionError("integer overflow")` when the `i64` result would overflow.
* **Rationale**: Modeling this faithfully would require re-deriving `Val.int` as an `i64`-bounded type and threading range side-conditions through every arithmetic lemma, for a failure mode with no interesting information-flow or termination consequences. The divergence is one-directional and safe: an execution the model treats as succeeding may in reality raise `ExecutionError` in the Rust runtime, never the reverse, so any proved property of a *successful* arithmetic result remains sound — it just doesn't guarantee the Rust op stays on the success path.
* **History**: Before RM-06, this gap was non-uniform in a dangerous way — `Mul` used `saturating_mul`, silently returning a numerically wrong (not merely error-omitted) result, while `Add`/`Sub` errored on overflow. RM-06 switched `Mul` to `checked_mul` so all three operators now uniformly signal `ExecutionError` on overflow in Rust, matching the uniform (if incomplete) treatment already present in the model.

---
*No proofs in the `formal/` development rely on unstated assumptions outside this ledger. To bridge this gap computationally, `Kani` and `Creusot` should verify the corresponding Rust kernel functions.*

### Trusted Kernels

The following Rust components represent the "kernel" of the trusted base. While their logic is modeled, their implementation correctness must be verified directly against the Rust code. We do not currently attempt to prove Rust type-level properties around the async runtime itself, thread isolation, or resource lifetimes within the Lean 4 model, as those are handled by the Rust compiler.

1. **`core::types::eval::check_type` (Phase A) & `parser::typecheck::infer` (Phase B/D)**: Kani harnesses (`kani_proofs` modules) exist in the source code to prove no-panics, static/dynamic agreement, and model agreement against `Differential.lean`. However, these are currently infeasible to verify iteratively on Windows developer machines due to Kani lacking native Windows support. They must be executed via CI or WSL environments.
2. **Flow Checker**: The `check_information_flow` logic is verified against the `Flow.lean` model (see `ROADMAP.md`). As with Phase A/B, Kani verification for these functions is currently unsupported on native Windows and must be run via Linux-based CI or WSL.
3. **Type System Enforcement**: The static type system is enforced at load time by `parser::typecheck::check_types` and dynamically evaluated by `core::types::eval::check_type`. This enforcement is modeled and proved in Lean via `evalE_preservation_lit`, `evalE_preservation_var`, and the `T4_type_soundness_lit` blueprint.
