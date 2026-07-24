# Formal Verification Results

This document enumerates the theorems established by the `λ_mizu` mechanized development in Lean 4 and their corresponding security invariants.

## Interfaces
These ITP proofs verify the **design**: the checker, as a mathematical function, is sound with respect to a cost-aware semantics. They answer: "Is the invariant right, and does `accept` imply it?"

A sibling follow-up using Kani/Creusot will prove the **code**: that the shipped Rust kernel functions accurately implement those modeled checks. The executable reference's `.mizu` examples serve as the practical model-fidelity check to ensure the model's predicted result matches the runtime's on every example.

## Theorems

### T1 — Bounded reaction (The honest termination result)
* **Status**: Proved (`eval_out`, `eval_nf`, `reaction_work_le` in `MizuFormal/Budget.lean`)
* **Prose**: For every well-formed reaction and every input, evaluation reaches a value, a defined error, or a `BudgetExceeded` result within the instrumented budget. The total resource consumption (expression steps and synthetic layout nodes) is `≤ MAX_INSTRUCTIONS + MAX_SYNTHETIC_LAYOUT_NODES`, which is a data-independent bound that relies on the document's structure rather than the size of any remote data.
* **Dependency**: This proof explicitly rests on the `MAX_INSTRUCTIONS` and `MAX_SYNTHETIC_LAYOUT_NODES` budgets. A cost-free structural termination result would be misleading, as `each` amplifications can cause unbounded layout blowup while remaining acyclic.
* **Mapping**: Discharges the "it can't freeze" manifesto property.

### T2 — Flow-checker soundness / non-interference
* **Status**: Proved (`T2_non_interference` in `MizuFormal/NonInterference.lean`, composing `eval_agree` through `recomputeStep_agree`, `execAction_agree`, `fireTx_agree`/`fireSubmit_agree`, and `reaction_agree`/`run_agree` — no `sorry`/`axiom` anywhere in the chain)
* **Prose**: For every document `d` where `accept(d) = true`, in the operational semantics, no `Untrusted`-labeled value reaches a sink without passing a gate. Stated as non-interference: two executions of `d` differing only in the values delivered by untrusted sources (`netResponse` and `submit` fields) emit the exact same sequence of ungated capability requests (the destinations reached without a gate are independent of untrusted input), and end in stores that agree on every untainted variable.
* **Mapping**: Discharges invariant **F** in `SECURITY-INVARIANTS.md`. It explicitly connects to the **N1–N5** navigation rules and the `path_param` gate validation.

### T3 — Checker completeness stance
* **Status**: Documented/Precision Witnessed (By Design)
* **Prose**: The flow checker evaluates soundness-over-completeness. It may reject safe documents through over-approximation (e.g., unused variable writes), adopting an "extra dependencies harmless" philosophy similar to the `comp` logic. However, it will never accept an unsafe document.

### T4 — Type soundness
* **Status**: Partial (Blueprint Only)
* **Prose**: Type soundness (progress and preservation) is modeled via the type system predicate `ValHasType` and environment conformance `storeConforms`. However, the full mutual induction is currently deferred. Per-constructor preservation is established (e.g., `evalE_preservation_lit`, `evalE_preservation_var`), and `T4_type_soundness_lit` acts as a blueprint for the complete theorem.
* **Unblocking**: The partial type system was strengthened with explicit typing rules and parameter annotations. Full mutual induction remains deferred pending deeper list/record proof extensions.
