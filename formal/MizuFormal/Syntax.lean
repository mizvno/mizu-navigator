/-!
# λ_mizu — Syntax

Deep embedding of the trust-relevant core of the Mizu language.

Mirrors (commit `dfd13bd` + working tree). Citations below name the Rust
item (`file.rs::item`) rather than a line number — see the citation
convention note near the end of this file for why.
* `src/parser/logic.rs::Expr`
* `src/parser/logic.rs::Action`
* `src/parser/logic.rs::MizuFunction`, `src/parser/logic.rs::ComputedBinding`
* `src/core/types.rs::Value` — has no floating-point variant in this version
  of the code at all (not merely omitted from the model here); see
  `FIDELITY.md` §V1

## Binder representation

`Let` and function parameters bind.  We use **named symbols with runtime
environments** — *not* De Bruijn indices or locally-nameless.  Rationale: the
Rust evaluator (`StateMachine::evaluate`) performs no substitution at all; it
resolves `Expr::Variable` by name against a local stack (innermost binding
wins, locals shadow globals) and a global store.  A named-environment
semantics is therefore the *faithful* model, and it eliminates the entire
substitution metatheory: no α-conversion, no substitution lemmas.  The price —
lemmas quantify over environments — is exactly the shape the
non-interference proof needs anyway.
-/

namespace Mizu

/-- Interned identifier (mirrors `core::types::Symbol(u32)`).  The interner is
frozen after parse; the model reflects this by never minting symbols at
runtime: every `Symbol` in a run is drawn from the (finite) set occurring in
the `Doc`, the store, or the events. -/
abbrev Symbol := Nat

/-- Types — mirrors `parser::logic::ValueType` (`logic.rs::ValueType`), added in Phase A. -/
inductive Ty where
  | num | str | bool
  | list     : Ty → Ty
  | record   : List (String × Ty) → Ty  -- closed row, canonically sorted
  | nullable : Ty → Ty
  deriving Repr, BEq

/-- Runtime values.  Mirrors `core::types::Value`, which has no
floating-point variant of its own to mirror (`FIDELITY.md` §V1), and
`Record` as an association list rather than a `BTreeMap` (`FIDELITY.md`
§V2). -/
inductive Val where
  | null   : Val
  | bool   : Bool → Val
  | int    : Int → Val
  | str    : String → Val
  | list   : List Val → Val
  | record : List (String × Val) → Val
  deriving Repr, Inhabited, BEq

/-- Binary operators — mirrors `parser::logic::BinOp` (`logic.rs::BinOp`). -/
inductive BinOp where
  | add | sub | mul | div
  | eq | ne | lt | gt | le | ge
  | and | or
  deriving Repr, BEq, DecidableEq

/-- Expressions — mirrors `parser::logic::Expr` (`logic.rs::Expr`) clause by
clause.  A read-only tree: no mutation node, no loop node. -/
inductive Expr where
  /-- `Expr::Literal` -/
  | lit    : Val → Expr
  /-- `Expr::Variable` -/
  | var    : Symbol → Expr
  /-- `Expr::BinaryOp` -/
  | binop  : BinOp → Expr → Expr → Expr
  /-- `Expr::FunctionCall` -/
  | call   : Symbol → List Expr → Expr
  /-- `Expr::Let { name, value, body }` -/
  | letE   : Symbol → Expr → Expr → Expr
  /-- `Expr::Not` -/
  | not    : Expr → Expr
  /-- `Expr::IfElse` — lazy: only the selected branch is evaluated. -/
  | ite    : Expr → Expr → Expr → Expr
  /-- `Expr::FieldAccess` -/
  | field  : Expr → String → Expr
  deriving Repr, Inhabited

/-- HTTP verb — payload-irrelevant to the theorems; kept for trace fidelity. -/
inductive Method where
  | get | post | put | delete | query
  deriving Repr, BEq, DecidableEq

/-- Actions — mirrors `parser::logic::Action` (`logic.rs::Action`).
`Assign.target` and `NetworkCall.target_var` are `String` in Rust and
resolved against the frozen interner at runtime; the model uses the
already-interned `Symbol` (`FIDELITY.md` §A1). -/
inductive Action where
  /-- `Action::Eval` — an expression evaluated for its effects. -/
  | eval        : Expr → Action
  /-- `Action::Assign` -/
  | assign      : Symbol → Expr → Action
  /-- `Action::Navigate` -/
  | navigate    : Expr → Action
  /-- `Action::NetworkCall { method, alias_sym, payload, path_param, target_var }` -/
  | networkCall : Method → Symbol → Option Expr → Option Expr → Symbol → Action
  deriving Repr, Inhabited

/-- A compiled function — mirrors `MizuFunction` (`logic.rs::MizuFunction`).
Type annotations on parameters are now mandatory (`FIDELITY.md` §A2, and T4 in
`RESULTS.md`), modeled by the `Ty` field. -/
structure FunDef where
  params : List (Symbol × Ty)
  body   : Expr
  deriving Repr, Inhabited

/-- A computed (derived) variable — mirrors `ComputedBinding`
(`logic.rs::ComputedBinding`). The static `depends_on` list is *computed* by
the model (`Semantics.compDeps`) rather than stored, mirroring what
`parse_computed_with_functions` derives (`FIDELITY.md` §A3). -/
structure CompDef where
  name : Symbol
  expr : Expr
  deriving Repr, Inhabited

/-- The builtins implemented by the evaluator dispatch
(`core::types::StateMachine::evaluate_impl`, `types.rs::StateMachine::evaluate_impl`).

`get_system_time` (RM-04): its target-variable used to be a *runtime-evaluated
string*, giving untrusted data a write-target selection channel that broke
the static-target assumption every other write in this model relies on —
previously excluded from the model for exactly that reason. The Rust fix
(`parser::logic.rs`'s call-argument parser) now requires the argument to be a
bare identifier, resolved to a `Symbol` at parse time and never evaluated —
structurally identical to `download`'s alias argument — so it is included
here the same way, via `evalGetSystemTime` (`Semantics.lean`). -/
inductive Builtin where
  | storeLocal | copyClipboard | download | getSystemTime | filter | count | sort
  deriving Repr, BEq, DecidableEq

/-- One `each` layout node, reduced to what the node budget sees:
which list variable it iterates and the size of its template subtree
(`layout_bridge.rs::expand_each_nodes`). -/
structure EachSpec where
  listVar      : Symbol
  templateSize : Nat
  deriving Repr

/-- A parsed, load-checked document — the model's unit of trust.

Handlers are indexed lists (the runtime keys click/submit actions by node id;
the model by position — `FIDELITY.md` §D1).  `declared` is the domain of the
frozen interner restricted to variables (`set_runtime` drops writes to
undeclared names, `types.rs::VariableStore::set_runtime` — note: not in
`logic_worker.rs`; `set_runtime` is a `VariableStore` method in
`core::types`, only *called* from `logic_worker.rs`).  `builtinOf` is the
static name→builtin dispatch table (name resolution against the frozen
interner). -/
structure Doc where
  functions : List (Symbol × FunDef)
  comps     : List CompDef          -- in parsed (topological) order
  timers    : List Action           -- root-timer actions, by timer index
  clicks    : List Action           -- click handlers, by handler index
  submits   : List Action           -- submit handlers, by handler index
  urls      : List Symbol           -- alias symbols in the `urls` registry
  declared  : List Symbol           -- interned variable names
  formSym   : Symbol                -- the interned `$form`
  builtinOf : Symbol → Option Builtin
  eachSpecs : List EachSpec

/-- Symbols of computed variables (assignment to these is rejected at runtime,
the `Action::Assign` arm of `execute_action`, `logic.rs::execute_action`). -/
def Doc.compSyms (D : Doc) : List Symbol := D.comps.map (·.name)

/-- Errors — mirrors the subset of `MizuError` reachable from evaluation. -/
inductive Err where
  /-- `MizuError::Timeout` — the instruction budget tripped. -/
  | timeout      : Err
  /-- Model artifact: structural fuel exhausted.  Proven unreachable under the
  budget discipline (`Budget.lean`, fuel adequacy). -/
  | fuel         : Err
  /-- `MizuError::ExecutionError("evaluation nesting too deep (max 256
  levels)")` — mirrors the `eval_depth` guard in `StateMachine::evaluate`
  (`types.rs::StateMachine::evaluate`), which is checked *after* the
  instruction-budget charge and is independent of it: it exists to bound
  native call-stack depth (`MAX_EVAL_DEPTH`, `types.rs::MAX_EVAL_DEPTH`),
  not interpreter work. Distinct from `Err.fuel` (a model-only termination
  artifact, RM-09): this case is reachable and models a real Rust error
  path. -/
  | evalDepthExceeded : Err
  /-- `MizuError::VariableNotFound` -/
  | varNotFound  : Symbol → Err
  /-- `MizuError::TypeError` -/
  | typeError    : Err
  /-- `MizuError::DivisionByZero` -/
  | divByZero    : Err
  /-- `MizuError::ExecutionError` and other defined failures. -/
  | execError    : String → Err
  deriving Repr, BEq, DecidableEq

/-- Capability effects queued during evaluation — mirrors
`network::RuntimeAction` as emitted by the logic worker (before UI-side
resolution).  `navigate`/`networkCall` are emitted only by `execAction`
(action position); expression position can emit only the other four —
this asymmetry is load-bearing for T2 and mirrors the Rust dispatch
(`types.rs::StateMachine::evaluate_impl` vs `logic.rs::execute_action`).

`getSystemTime target` mirrors `RuntimeAction::GetSystemTime`: like
`networkCall`, the queued request is resolved *later* by the host
environment (real wall-clock time, delivered back via the same
`UiEvent::UpdateVariable` path a network response uses — `render/security.rs`)
rather than synchronously, which is why it is an effect and not a direct
store write here (expression evaluation never writes `σ` in this model —
see `evalE`'s doc comment in `Semantics.lean`). The delivered value is never
attacker-influenced, so — like `storeLocal`/`copyClipboard`/`download`'s own
effects — its delivery is out of `T2_non_interference`'s scope; only that
the *target Symbol* is now static (this file) and load-time-checked against
`comp` collisions (`parser::flow.rs`) is claimed. -/
inductive Effect where
  | navigate      : String → Effect
  | networkCall   : Method → Symbol → Option String → Option Val → Symbol → Effect
  | storeLocal    : String → Val → Effect
  | copyClipboard : String → Effect
  | download      : Symbol → Effect
  | getSystemTime : Symbol → Effect
  deriving Repr, BEq

/-- `true` iff the effect is a navigation request. -/
def Effect.isNav : Effect → Bool
  | .navigate _ => true
  | _           => false

/-- Trigger context of a reaction — mirrors `flow::ActionContext`. -/
inductive Ctx where
  | gesture        -- click / submit (gate G1 discharges navigation taint)
  | nonInteractive -- root timer / network-response delivery
  deriving Repr, BEq, DecidableEq

/-- Input events driving a run — mirrors `logic_worker::UiEvent` (the
reaction-relevant subset).  `netResponse` is the delivery of a network
response (`UiEvent::UpdateVariable`); `submit` carries the form fields.
Both are the *untrusted* inputs of T2. -/
inductive Event where
  | click       : Nat → Event
  | timer       : Nat → Event
  | submit      : Nat → List (String × Val) → Event
  | netResponse : Symbol → Val → Event
  deriving Repr

/-!
## Citation convention (RM-16)

Every comment in `formal/MizuFormal/*.lean` that points at a specific piece
of Rust source cites it as `` `file.rs::item` `` — the file plus the
function, method (`file.rs::Type::method`), struct, enum, or constant name —
**never** a bare line number or line range (`` `file.rs:1234` ``,
`` `file.rs:1234-1240` ``).

**Why.** An RM-16 audit found that most of the line-number citations in this
directory had silently gone stale: normal Rust changes (new match arms,
inserted validation, a moved helper) shift line numbers throughout a file
without changing the functions themselves, and nothing re-checks a comment
against the code it describes. One case (`pathParamOk`'s citation in
`Semantics.lean`, found in the original security review that started this
remediation series) had drifted far enough to point at the `Mul`/`Div`
arithmetic arms of `apply_binop` — unrelated code — which helped hide a real
gap (the runtime validation gate `pathParamOk`/`path_param_ok` mirrors
didn't exist yet) behind a citation that *looked* precise. A name survives
exactly the class of refactor that breaks a line number: the function can
move anywhere in the file, or to a different line count entirely, and the
citation is still correct as long as the name and its general behavior
haven't changed. It can still go stale if the *named item itself* is renamed,
split, or removed — that failure mode is easy to catch (the name simply
won't grep-match anymore), unlike a line number, which always "matches"
something, just not necessarily the right thing.

When a cited Rust construct doesn't have its own name (e.g. one match arm
inside a large function, or one loop inside another function), cite the
enclosing named item plus a short description of the arm/loop in prose
(e.g. `` the `"store_local"` arm of `types.rs::StateMachine::evaluate_impl` ``)
rather than a line span within it.
-/

end Mizu
