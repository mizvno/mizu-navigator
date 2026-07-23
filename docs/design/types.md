# Design memo — Structural types for parameters & collections (ux-8)

**Status: APPROVED (signed off).** All four open questions decided as written
below: closed-row records (YAGNI), case-sensitive exact field matching,
high-fidelity Phase D error messages matching the `urls.rs` bar, and a
**hard-break migration** — Phase C (staged audit/migration) is **waived**;
Phase D now directly rewrites the fixtures when it lands. No Rust or Lean
source has been touched yet. Phase 2 implements exactly the decisions below.

This memo is Phase 1 of ux-8 (two-phase, same discipline as ux-6/ux-7: design,
then implement only after approval). It decides *what* the type algebra, the
checking strategy, and the rollout sequence are — not how they're coded. Phase 2
will cite this file's decisions as its spec. It is the Phase-1 deliverable for
`ROADMAP.md`'s own follow-up #2 ("type `Record`/`Null`, annotate parameters, so
a soundness statement becomes expressible") and directly unblocks **T4** in
`formal/RESULTS.md`.

## Verified starting state

Everything below was read from the current tree this session, not assumed:

- **A shallow type-annotation mechanism already ships today** — this is an
  extension, not a green field. `MizuFunction.params : Vec<(Symbol,
  Option<ValueType>)>` (`ast.rs:246`); `ValueType` today has exactly four
  variants — `Num | Str | Bool | List` (`ast.rs:13-22`); `parse_params`
  (`parse.rs:383-421`) already parses `name: type` syntax, optional per
  parameter. It is in production use: `docs/reference/examples/02_logic_basics.mizu:4`
  ships `double(x: num)`.
- **`parse_params` already knows it's incomplete and says so.** Its own doc
  comment (`parse.rs:372-376`) states writing `dict`, `record`, or `any`
  "produces a `ParseError`... use an unannotated parameter instead" — Record
  typing is a known, deliberately deferred gap, not an oversight.
- **`check_type` (`eval.rs:238-263`) is a nominal tag check, not a type
  system.** `ValueType::List` matches iff the value is *a* `Value::List` —
  nothing constrains element type. There is no `ValueType::Record` at all.
  `Value::List(Arc<Vec<Value>>)` (`value.rs:15-26`) is structurally
  heterogeneous — nothing stops `[1, "a", true]` at construction. `check_type`
  never matches `Value::Null` against any expected variant, so **passing
  `Null` into an already-annotated parameter is already a `TypeError` today** —
  worth stating precisely because it de-risks Decision 3 below.
- **`FieldAccess` has zero static field information.** `eval.rs:751-763`
  checks only that the base is *a* `Record`, then does a runtime
  `binary_search` for the field, raising `VariableNotFound` if it's absent.
  No caller anywhere knows a record's shape ahead of time.
- **`ValueType` is used nowhere except function-parameter call sites.**
  Grep-confirmed zero occurrences outside `ast.rs` / `parse.rs` /
  `eval.rs::check_type`. `comp` and `let` bindings carry no type information
  at all today.
- **The Lean model is further behind than the Rust code.**
  `Syntax.lean`'s `FunDef` (`Syntax.lean:101-104`) is `params : List Symbol` —
  it doesn't even mirror today's *optional* `ValueType`. `T4` in `RESULTS.md`
  (lines 27-30) is `Open`, blocked on exactly this.
- **The flow checker (`flow.rs`) is a separate, verified-orthogonal axis.**
  Its doc header (`flow.rs:1-21`) states it's a pure taint-propagation
  fixpoint over the DAG; its `FieldAccess` handling (`flow.rs:358`, `:405`,
  `:554`) only asks "does the base carry taint" — it has no notion of value
  shape or field existence. Confirmed by reading the source, not inferred:
  the new static type pass and the existing flow pass read the same `Expr`
  tree but compute on independent lattices (shape vs. trust-label), matching
  the "complementary axes, don't merge them" precedent ux-6's memo already
  established for logic-driven vs. environment-driven variants.
- **Migration surface is fully audited, not estimated.** Every `.mizu` fixture
  in the repo is exactly the 8 files under `docs/reference/examples/` plus 3
  `err_*.mizu` files (all driven by `tests/reference_examples.rs`, which
  embeds no additional source) and the one inline `logic` block in
  `tests/storage_rehydration_taint.rs:157-162` (no function definition at
  all). Across all of it, exactly **one** parameter exists —
  `02_logic_basics.mizu:4`'s `x: num` — and it's already annotated. This
  bounds the entire direct-rewrite blast radius Phase D takes on now that
  Phase C is waived (Decision 5): one already-typed parameter, nothing else.

## Decision 1 — Type algebra

```
Ty ::= Num | Str | Bool
     | List(Ty)
     | Record({ name: Ty, ... })     -- closed row
     | Ty "?"                         -- explicit nullable qualifier
```

- **Closed-row records only in v1** (exact field set, name-matched, no
  open/structural subtyping). Decidable, needs no row-variable machinery in
  either Rust or Lean, and is sufficient to type `FieldAccess` soundly — the
  type of `r.field` is simply looked up in the record's `Ty`. **Explicit
  non-goal, not silently foreclosed:** a function accepting "any record with
  at least field X" (width subtyping / open rows) is real future work if a
  concrete need emerges; this memo does not build toward it.
- **`List(Ty)` is a single homogeneous element type.** No variance rules
  needed beyond what `Nullable` requires, since Mizu has no other subtyping.
- **`Null` is an explicit `T?` qualifier, never implicit.** A bare `num`
  parameter must reject `Null` — which, per the starting-state audit above,
  **is already exactly what happens today** once a parameter is annotated at
  all. Nothing here narrows currently-annotated behavior; it only means that
  after Phase D (mandatory annotations), a parameter that today relies on
  being *unannotated* to accept `Null` as an "absent" signal must gain a `?`
  in Phase D's direct rewrite (Phase C's staged migration is waived — see
  Decision 5) — and the audited migration surface above shows there is no
  such parameter in the repo to worry about.
- **No function types.** Mizu has no closures or first-class functions
  (`FunctionCall` only names top-level functions, confirmed in `ast.rs`), so
  `Ty` has no reason to represent one.
- **Field-name matching is case-sensitive, exact.** Confirmed at sign-off,
  consistent with `Value::Record`'s existing `binary_search_by_key` compare.
  The type algebra stays pure on this point deliberately: if
  `network/worker/fetch.rs` ever needs to tolerate inconsistent JSON casing,
  that normalization belongs at the deserialization boundary, not as a
  case-folding rule smuggled into `Ty`'s equality.

**Confirmed at sign-off: closed-row, no width subtyping.** YAGNI — no current
use case needs "accept any record with at least field X," and closed rows
keep both the Rust implementation and the Lean `Preservation` proof strictly
monomorphic and decidable, with no row-variable machinery to build or prove
anything about.

**Source grammar** (parameters and record fields):

```
type   ::= "num" | "string" | "bool"
         | "list" "<" type ">"
         | "record" "{" field ("," field)* "}"
         | type "?"
field  ::= ident ":" type
```

Example: `f(x: num, y: list<num>, z: record{name: string, age: num?})`.

## Decision 2 — Checking happens statically, at load time; the runtime check stays as defense-in-depth

Today, `check_type` runs dynamically — once, at the moment a function is
actually *called* (`eval.rs:702`). That is a contract check, not type
soundness: an unreachable branch with a mismatched argument would never be
caught. To make T4 a real static claim, add a **new load-time static pass**,
architecturally parallel to `flow.rs::check_information_flow` — same
shape: whole-document, runs before the document is considered ready,
fail-secure (any analysis uncertainty rejects). This mirrors the project's
own established layering: the frozen interner keeps its runtime
`set_runtime` guard even though the flow checker statically proves no
untrusted-symbol injection is possible; the same "prove it statically, keep
the cheap runtime guard anyway" discipline applies here. `check_type` is
**not removed** — it remains exactly as today, now redundant-by-proof for
well-typed documents but still the only guard for anything the static pass
couldn't reach (there should be nothing, once Phase D lands, but "prove it,
then keep the belt too" costs nothing).

## Decision 3 — Bidirectional checking, not unification-based inference

Full Hindley-Milner (type variables, unification, generalization) is more
machinery than this language needs: there is no let-polymorphism requirement,
and parameters are annotated by construction. **Bidirectional typing**
(`infer` synthesizes a type bottom-up from annotated leaves; `check` verifies
an expression against an expected type) is decidable by construction and maps
directly onto `ast.rs`'s existing `Expr` constructors:

- **Parameters** are annotated — the base case for `infer`.
- **Function bodies** are synthesized bottom-up (`Let` → `BinaryOp` →
  `IfElse` → `FieldAccess` → `FunctionCall`, …) — **no return-type annotation
  is required**; a function's signature is simply `params -> infer(body)`.
- **`comp`/`let` bindings are synthesized, never annotated** — keeps the
  breaking change minimal and non-viral: only function *parameters* ever need
  a written type.
- `Ty` stays **monomorphic** everywhere it's stored — no type variable ever
  appears inside a `Ty` value in Rust or Lean, which keeps both the
  implementation and the eventual Preservation proof short (a monomorphic
  system's preservation argument is materially simpler than one with
  unification).

## Decision 4 — Builtin polymorphism is schematic, not object-level generic

`filter`, `count`, `sort` operate over `list<T>` for arbitrary `T`. Rather
than adding real parametric polymorphism to `Ty` (generalization/instantiation
machinery — exactly what Decision 3 avoids), each builtin's typing rule is
stated with a **schematic meta-variable** `T`, quantified in the *rule*, not
in the object language — the same way a Lean lemma can be stated generically
about `Val` without `Val` itself being polymorphic. `filter : (T -> Bool) ->
List(T) -> List(T)` is instantiated per call site by unifying `T` with the
caller's concrete argument type; no `Ty::Var` constructor is ever needed.

## Decision 5 — Rollout: the breaking change is isolated to one phase

Best practice for a change like this is small, independently-landable,
independently-testable increments, with the actual source-language break
confined to the one commit that can't be avoided — mirroring how ux-7 called
out its one unavoidable breaking rename (`direction` → `flex-direction`) as a
single, explicit, isolated commit rather than smearing it across the diff.

- **Phase A — additive, non-breaking (Rust).** Extend `ValueType`/`Ty` to the
  full algebra above and add the grammar to `parse_params`. Annotations stay
  optional, exactly as today. Independently landable; zero behavior change
  for any undecorated document.
- **Phase B — additive, non-breaking (Rust).** New load-time static pass
  (e.g. `src/parser/logic/typecheck.rs`), architecturally beside `flow.rs`.
  An unannotated parameter is treated as an explicit "dynamic" escape hatch —
  the static check trivially passes, `check_type` remains the only guard for
  that binding, exactly as today. This lets the checker ship, get its own
  test suite, and run against real documents before anyone is forced to
  migrate anything.
- **Phase C — WAIVED at sign-off.** Decided: no staged audit/migration phase.
  Backward compatibility for `.mizu` source is explicitly a non-goal at this
  stage — there is no production deployment and no external `.mizu`
  ecosystem to protect. The cautious "annotate, then verify under Phase B's
  checker before breaking anything" step this phase would have been is
  cancelled; its one piece of useful output (the audited migration surface —
  see the starting-state note above) is folded directly into Phase D below.
  If a fixture breaks when Phase D lands, it gets rewritten then, not staged
  ahead of time.
- **Phase D — the breaking change, direct rewrite, no staging.** Flip
  `parse_params` to require a `: type` annotation on every parameter and
  delete Phase B's "trivially accept untyped" branch, in the same commit that
  directly rewrites whatever fixtures the audited surface above still leaves
  unannotated at that time (today: none — `02_logic_basics.mizu:4` is already
  typed). This is the **only** commit in the plan that changes what
  currently-valid `.mizu` source parses. **Error-message bar, confirmed at
  sign-off: high-fidelity, not a generic `TypeError`.** Every rejection names
  the offending parameter and lists the full valid grammar inline — `num`,
  `string`, `bool`, `list<T>`, `record{...}`, and the `?` nullable suffix —
  matching `urls.rs`'s existing standard of naming exactly what's wrong and
  exactly what's expected, not just that something is.
- **Phase E — Lean model, mirrors A–D.** Add `Ty` to `Syntax.lean`; change
  `FunDef.params` from `List Symbol` to `List (Symbol × Ty)`
  (`Syntax.lean:101-104`); add a `HasType` typing judgment over every `Expr`
  constructor, `FieldAccess` in particular (record-shape lookup). Cite every
  Rust anchor by name per the RM-16 convention (`Syntax.lean:240-271`) from
  the first line of this phase's diff — don't repeat the citation-drift
  incident RM-16 exists to prevent. Prove Preservation and Progress
  incrementally, one lemma per `Expr` constructor, then compose into
  `T4_type_soundness`, mirroring `T2_non_interference`'s own compositional
  structure (`eval_agree` → `recomputeStep_agree` → `execAction_agree` →
  `fireTx_agree`/`fireSubmit_agree` → `reaction_agree`/`run_agree`). Note:
  changing `FunDef.params`'s Lean type is a **mechanical refactor** of every
  site that constructs/destructures `FunDef` in the T1/T2 proof files — not
  new proof work, and not reopening either theorem, but budget the plumbing.
- **Phase F — Kani/Creusot bridge.** `FIDELITY.md`'s closing line already
  commits the project to verifying kernel functions this way; the new static
  checker (and `check_type`) become two more. Verify no panics, and that
  accept/reject decisions agree with `HasType` on the differential
  model-fidelity corpus `RESULTS.md` already describes.
- **Phase G — documentation.** `FIDELITY.md` §4 ("Type Annotations") is
  currently written as an accepted *idealization* (the model omits what the
  code doesn't enforce); once Phase D lands this is no longer true and the
  entry should move out of "Abstractions and Idealizations" entirely.
  `RESULTS.md`'s T4 flips from `Open` to `Proved` with the new lemma names
  cited. `docs/reference/grammar.md` / `semantics.md` / `tutorial/index.md`
  gain the new type grammar. Add one line to `SECURITY-INVARIANTS.md` stating
  the new pass and the flow checker are verified-independent (per this memo's
  starting-state note on `flow.rs`) — write it down even though the answer is
  "no interaction," per the project's own RM-16 lesson that unstated
  assumptions are exactly what silently goes stale.

## Security posture

Orthogonal to information flow, verified rather than assumed: `HasType`
assigns *shape*; `check_information_flow` assigns *trust label*
(Trusted/Untrusted). Neither pass consumes the other's output — confirmed by
reading `flow.rs`, whose `FieldAccess` handling only asks "does the base
carry taint," never "what shape is the base." A value can be well-typed and
tainted, or (before Phase D, transitionally) untyped and untainted; the two
lattices commute. No new capability, sink, or gate is introduced anywhere in
this plan — the new pass rejects documents at load time, exactly like
`flow.rs` and `check_dag` already do, and rejecting is always the fail-secure
direction.

## Decisions confirmed at sign-off

1. **Closed-row vs. open-row records:** **closed-row**, approved as written
   (Decision 1). YAGNI — no current use case requires width subtyping;
   closed rows keep the Rust implementation and the Lean `Preservation`
   proof strictly monomorphic and decidable, no row variables anywhere.
2. **Record field-name matching:** **case-sensitive exact match**, approved
   as written (Decision 1), matching the existing `binary_search_by_key`
   behavior. The type system stays pure on this point deliberately: if
   `network/worker/fetch.rs` needs to tolerate inconsistent JSON casing on
   ingest, that normalization is that module's problem at the
   deserialization boundary, not a reason to complicate `Ty`'s equality.
3. **Phase D error-message bar:** **high-fidelity, mandatory**, approved as
   written (Decision 5). Generic `TypeError`s are rejected outright; every
   Phase D rejection must name the parameter and enumerate the full valid
   grammar (`num`, `string`, `bool`, `list<T>`, `record{...}`, `?`), matching
   `urls.rs`'s existing standard.
4. **Migration strategy: hard break, Phase C waived.** Backward compatibility
   for `.mizu` source is explicitly **not a goal** at this stage — no
   production deployment, no external `.mizu` ecosystem exists yet. The
   staged audit/migration phase is cancelled; Phase D directly rewrites
   whatever fixtures need it when it lands (today, per the audited starting
   state above, that's zero files — the repo's one parameter is already
   typed). Decision 5 above is rewritten to reflect this; there is no
   re-audit step to schedule.

## Cross-references

- `ROADMAP.md` follow-up #2 — this memo is that follow-up's Phase 1.
- `FIDELITY.md` §4 (Type Annotations) — superseded by Phase G.
- `RESULTS.md` T4 — target of Phases E/F.
- `SECURITY-INVARIANTS.md` — cross-check added in Phase G.
- ux-6 (`responsive.md`) — precedent for "complementary axes, don't merge
  them," reused above for the type-pass/flow-pass independence argument.
- ux-7 (`bidi.md`) — precedent for this memo's format and for isolating an
  unavoidable breaking change to a single, explicitly-flagged commit.

---

**Phase 1 complete and signed off.** Phase 2 (Phases A, B, D, E, F, G above —
Phase C waived per Decision 5) proceeds next, in separate commits, each
implementing exactly the decisions above.
