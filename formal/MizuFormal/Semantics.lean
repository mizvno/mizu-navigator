import MizuFormal.Syntax

/-!
# λ_mizu — Cost-instrumented operational semantics

Mirrors (citations name the Rust item, `file.rs::item`, rather than a line
number — see the convention note at the end of `Syntax.lean`):
* `core::types::StateMachine::evaluate` / `evaluate_impl`
  (`types.rs::StateMachine::evaluate`, `types.rs::StateMachine::evaluate_impl`)
  — entry charge, per-native pre-charges, effect accumulation;
* `parser::logic::execute_action` (`logic.rs::execute_action`)
  — per-action budget reset, the `path_param` A1+A2 gate (G2) via
  `parser::logic::path_param_ok` (`logic.rs::path_param_ok`);
* `parser::logic::recompute_computed_bindings` (`logic.rs::recompute_computed_bindings`)
  — per-comp budget reset, error-skipping, cascade through `changed`;
* `parser::logic_worker::LogicWorker::run_loop` / `execute_and_respond`
  (`logic_worker.rs::LogicWorker::run_loop`,
  `logic_worker.rs::LogicWorker::execute_and_respond`) — the reaction
  discipline: transactional rollback for click/timer, `$form` population for
  submit, declared-only writes for network-response delivery.

## Cost model

The reduction is instrumented with two counters threaded in `EvalSt`:

* `count` mirrors `instruction_count`: every evaluator entry adds 1, native
  list builtins pre-charge `len` (or `n·(⌊log₂ n⌋+1)` for `sort`) *before*
  doing the O(n) work, and any charge that pushes `count` past the budget `B`
  aborts with `Err.timeout` (`MizuError::Timeout`).
* `work` is model-only instrumentation: it counts the units of computation
  actually *performed*, and is incremented only when the corresponding charge
  passed the check.  T1 proves `work ≤ B` — the budget really does bound the
  work done, not just label it.

Structural termination is by a fuel parameter.  Fuel is **not** part of the
modeled language: `Budget.lean` proves that with `fuel = B + 2` the
`Err.fuel` outcome is unreachable, because every recursion step costs at
least one instruction and the instruction check trips first.
-/

namespace Mizu

/-! ## Stores and environments -/

/-- Local environment: innermost binding first (mirrors the local stack with
innermost-wins lookup, `types.rs::StateMachine::get_local`). -/
abbrev Env := List (Symbol × Val)

/-- Global store (mirrors `global_store`).  Updates prepend; lookup takes the
first match, so observational behaviour equals the Rust map. -/
abbrev Store := List (Symbol × Val)

/-- First-match association lookup. -/
def alookup [BEq α] : List (α × β) → α → Option β
  | [], _ => none
  | (k, v) :: rest, s => if k == s then some v else alookup rest s

def setVar (σ : Store) (s : Symbol) (v : Val) : Store := (s, v) :: σ

/-- Callee environment for a user-function call: parameters bound in order,
last parameter innermost (mirrors `push_local` in order + last-wins lookup). -/
def bindParams (params : List Symbol) (vals : List Val) : Env :=
  (params.zip vals).reverse

/-! ## Evaluator state and charging -/

structure EvalSt where
  count     : Nat
  work      : Nat
  effects   : List Effect
  /-- Current `evalE` recursion depth; see [`MAX_EVAL_DEPTH`] (RM-09).
  Mirrors `StateMachine.eval_depth` (`types.rs::StateMachine::eval_depth`
  field): incremented on entry to `evalE` and decremented before every
  return, so — unlike `count`, which only ever grows — it tracks live
  nesting, not cumulative calls. -/
  evalDepth : Nat
  deriving Repr

def EvalSt.init : EvalSt := ⟨0, 0, [], 0⟩

abbrev Res (α : Type) := Except Err α × EvalSt

/-- Charge `n` instructions.  Mirrors `instruction_count += n; if > MAX →
Timeout`, applied at each of `evaluate`/`evaluate_impl`'s native pre-charge
sites (`types.rs::StateMachine::evaluate`,
`types.rs::StateMachine::evaluate_impl`).  `work` advances only
when the check passes: work is performed strictly under the budget.
`evalDepth` is untouched — instruction budget and recursion-depth budget are
independent (RM-09). -/
def charge (B n : Nat) (s : EvalSt) : Res Unit :=
  if s.count + n > B then (.error .timeout, { s with count := s.count + n })
  else (.ok (), { s with count := s.count + n, work := s.work + n })

/-- Queue a capability effect (mirrors `accumulated_actions.push`). -/
def emit (eff : Effect) (s : EvalSt) : EvalSt :=
  { s with effects := s.effects ++ [eff] }

/-! ## Pure value operations -/

/-- Mirrors `apply_binop` (`logic.rs::apply_binop`) restricted to the
Float-free value space; non-exact integer division (which has no exact
`Int` result and `Value` has no floating-point variant to hold an inexact
one) is a modeled error (`FIDELITY.md` §V1).

**Known gap — unmodeled `i64` overflow (RM-06):** the Rust runtime's `Int` is
a fixed-point value backed by `i64`, and `Add`/`Sub`/`Mul` all use checked
arithmetic that returns `MizuError::ExecutionError("integer overflow")` on
overflow. `Val.int` here is Lean's unbounded `Int`, so `add`/`sub`/`mul`
below never fail — this is an accepted, deliberate incompleteness, not an
oversight: modeling it faithfully would mean re-deriving `Val.int` as an
`i64`-bounded type and threading range side-conditions through every
arithmetic lemma for a failure mode with no interesting information-flow or
termination consequences (overflow is *strictly more restrictive* than the
modeled behavior, i.e. an execution the model treats as succeeding may in
reality raise `ExecutionError` — never the other way around). Any theorem
here about the *result* of a successful arithmetic op is still sound; it
just doesn't guarantee the Rust op stays in the success path. Before RM-06,
this gap was non-uniform in an actively dangerous way: Rust's `Mul` used
`saturating_mul`, which silently returned a numerically **wrong** value
instead of erroring, while `add`/`sub` errored and the model matched
neither. RM-06 made all three operators uniformly checked in Rust, so the
gap here is now uniform too: every operator can diverge from the model only
via an unmodeled `ExecutionError`, never via a silently wrong result. -/
def applyBinop : BinOp → Val → Val → Except Err Val
  | .add, .int l, .int r => .ok (.int (l + r))
  | .add, .str l, .str r => .ok (.str (l ++ r))
  | .sub, .int l, .int r => .ok (.int (l - r))
  | .mul, .int l, .int r => .ok (.int (l * r))
  | .div, .int l, .int r =>
    if r == 0 then .error .divByZero
    else if l % r == 0 then .ok (.int (l / r))
    else .error (.execError "non-exact int division: Float omitted from model")
  | .eq, .int l, .int r => .ok (.bool (l == r))
  | .eq, .str l, .str r => .ok (.bool (l == r))
  | .eq, .bool l, .bool r => .ok (.bool (l == r))
  | .eq, .null, .null => .ok (.bool true)
  | .eq, .null, _ => .ok (.bool false)
  | .eq, _, .null => .ok (.bool false)
  | .ne, .int l, .int r => .ok (.bool (l != r))
  | .ne, .str l, .str r => .ok (.bool (l != r))
  | .ne, .bool l, .bool r => .ok (.bool (l != r))
  | .ne, .null, .null => .ok (.bool false)
  | .ne, .null, _ => .ok (.bool true)
  | .ne, _, .null => .ok (.bool true)
  | .lt, .int l, .int r => .ok (.bool (l < r))
  | .gt, .int l, .int r => .ok (.bool (l > r))
  | .le, .int l, .int r => .ok (.bool (l ≤ r))
  | .ge, .int l, .int r => .ok (.bool (l ≥ r))
  | .and, .bool l, .bool r => .ok (.bool (l && r))
  | .or, .bool l, .bool r => .ok (.bool (l || r))
  | _, _, _ => .error .typeError

/-- The real allocation cost of `applyBinop`, charged *before* the operation
runs — mirrors `parser::logic::apply_binop`'s pre-charge of `l.len() +
r.len()` for the `(Add, String, String)` case (`logic.rs::apply_binop`).
Every other operator/operand pair is O(1), matching Rust's flat per-AST-node
charge exactly (`0` extra beyond `evalE`'s entry `charge B 1`). -/
def binopCost : BinOp → Val → Val → Nat
  | .add, .str l, .str r => l.length + r.length
  | _, _, _ => 0

/-- `item is a Record ∧ item.field == target` (mirrors the closure in the
`filter`/`count` builtins, the `"filter"`/`"count"` arms of
`types.rs::StateMachine::evaluate_impl`). -/
def matchesField (f : String) (target : Val) (x : Val) : Bool :=
  match x with
  | .record m =>
    match alookup m f with
    | some v => v == target
    | none => false
  | _ => false

def filterField (xs : List Val) (f : String) (t : Val) : List Val :=
  xs.filter (matchesField f t)

def countField (xs : List Val) (f : String) (t : Val) : Nat :=
  (xs.filter (matchesField f t)).length

/-- Variant weight for heterogeneous comparison (mirrors `variant_weight`,
`types.rs::variant_weight`). Weight 4 is skipped: `variant_weight` itself
weighs six variants consecutively (1-6, no gap) since `Value` has no
floating-point variant to reserve a weight for; this model's `weight`
predates that clarification and keeps the historical gap at 4 rather than
renumbering — a cosmetic divergence with no effect on `cmpShallow`'s
ordering, since only relative order among these seven constructors matters,
not the absolute values. -/
def weight : Val → Nat
  | .null => 1 | .bool _ => 2 | .int _ => 3 | .str _ => 5 | .list _ => 6 | .record _ => 7

/-- Shallow field comparator (mirrors `compare_values`,
`types.rs::compare_values`, with list/record recursion flattened to the
variant-weight tiebreak — `FIDELITY.md` §B5). -/
def cmpShallow : Option Val → Option Val → Ordering
  | none, none => .eq
  | none, some _ => .lt
  | some _, none => .gt
  | some .null, some .null => .eq
  | some (.bool a), some (.bool b) => compare a b
  | some (.int a), some (.int b) => compare a b
  | some (.str a), some (.str b) => compare a b
  | some a, some b => compare (weight a) (weight b)

def Ordering.flip : Ordering → Ordering
  | .lt => .gt | .gt => .lt | .eq => .eq

def insertBy (le : Val → Val → Bool) (x : Val) : List Val → List Val
  | [] => [x]
  | y :: ys => if le x y then x :: y :: ys else y :: insertBy le x ys

/-- Deterministic sort by record field (mirrors the `sort_by` call in the
`"sort"` arm of `types.rs::StateMachine::evaluate_impl`; algorithm is
insertion sort — same comparator, possibly different equal-key order,
`FIDELITY.md` §B5). -/
def sortByField (f : String) (desc : Bool) (xs : List Val) : List Val :=
  let key (v : Val) : Option Val := match v with | .record m => alookup m f | _ => none
  let le (a b : Val) : Bool :=
    let o := cmpShallow (key a) (key b)
    let o := if desc then Ordering.flip o else o
    !(o == Ordering.gt)
  xs.foldr (insertBy le) []

/-- Rust: `usize::BITS - n.leading_zeros()` = `⌊log₂ n⌋ + 1` for `n > 0`,
the `sort` pre-charge in the `"sort"` arm of
`types.rs::StateMachine::evaluate_impl`. -/
def sortCost (n : Nat) : Nat := if n == 0 then 0 else n * (Nat.log2 n + 1)

/-- The `path_param` A1+A2 gate (G2): single segment, no `/` `\` `..`, no
ASCII control characters.  Mirrors `parser::logic::is_ctl`
(`logic.rs::is_ctl`). -/
def isCtl (c : Char) : Bool := c.val < 32 || c.val == 127

/-- Mirrors `parser::logic::path_param_ok` (`logic.rs::path_param_ok`).  Called
from `execute_action`'s `NetworkCall` arm before the value is queued as a
`RuntimeAction`, and re-validated in
`parser::logic_worker::resolve_endpoint_url` before substitution into the
resolved URL — every consumption point of `path_param` runs this gate. -/
def pathParamOk (s : String) : Bool :=
  !(s.any fun c => c == '/' || c == '\\' || isCtl c) && (s.splitOn "..").length == 1

/-! ## The evaluator -/

/-- download(alias): the argument must *syntactically* be a bare identifier;
it is never evaluated (the `"download"` arm of
`types.rs::StateMachine::evaluate_impl`).  The destination is a static
alias — capability confinement by construction. -/
def evalDownload (a : Expr) (s : EvalSt) : Res Val :=
  match a with
  | .var aliasSym => (.ok .null, emit (.download aliasSym) s)
  | _ => (.error (.execError "download: alias must be a bare identifier"), s)

/-- get_system_time(target): the argument must *syntactically* be a bare
identifier, never evaluated (the `"get_system_time"` arm of
`types.rs::StateMachine::evaluate_impl`) — the RM-04 fix.  Before
this, the argument was evaluated as an arbitrary expression to a string used
to look up the write target at runtime, giving untrusted data a channel to
select *which* variable gets overwritten; the model previously excluded
`get_system_time` entirely for exactly that reason (`Syntax.lean`).  Also
rejects a `comp` (computed) variable target, mirroring `execute_action`'s
existing `Assign`-to-`comp` guard (the `Action::Assign` arm of
`logic.rs::execute_action`) — load-time-checked too, by `parser::flow.rs`.
The delivered timestamp itself is not modeled;
see `Effect.getSystemTime`'s doc comment in `Syntax.lean` for why. -/
def evalGetSystemTime (D : Doc) (a : Expr) (s : EvalSt) : Res Val :=
  match a with
  | .var targetSym =>
    if D.compSyms.contains targetSym then
      (.error (.execError "get_system_time cannot target a computed variable"), s)
    else
      (.ok (.bool true), emit (.getSystemTime targetSym) s)
  | _ => (.error (.execError "get_system_time: target must be a bare identifier"), s)

/-- Maximum `evalE` recursion depth.  Mirrors `MAX_EVAL_DEPTH`
(`types.rs::MAX_EVAL_DEPTH`): bounds native call-stack depth, independent of
the instruction budget `B` (RM-09). -/
def MAX_EVAL_DEPTH : Nat := 256

mutual

/-- Expression evaluation — mirrors `StateMachine::evaluate` +
`evaluate_impl` (`types.rs::StateMachine::evaluate`,
`types.rs::StateMachine::evaluate_impl`) clause by clause.

Entry: `charge B 1` (instruction_count += 1, timeout check), **then** the
`evalDepth` guard — mirroring Rust's check order exactly: `eval_depth` is
only incremented/checked *after* the instruction-count check, in
`types.rs::StateMachine::evaluate`. Once the guard passes, `evalDepth` is incremented for
the duration of the recursive dispatch on `e` and decremented again on the
way out (`s1'` → final `sEnd` decrement), exactly mirroring `eval_depth += 1`
before / `eval_depth -= 1` after `evaluate_impl` in Rust — so, unlike `count`
(which only ever grows), sibling subexpressions (e.g. the two operands of a
`binop`) do not accumulate each other's depth, only true nesting does. The
store `σ` is read-only throughout (the Rust evaluator never writes
`global_store`); effects and counters thread through `EvalSt`. -/
def evalE (B : Nat) (D : Doc) (σ : Store) (fuel : Nat) (env : Env) (e : Expr)
    (s : EvalSt) : Res Val :=
  match fuel with
  | 0 => (.error .fuel, s)
  | fuel' + 1 =>
    match charge B 1 s with
    | (.error er, s1) => (.error er, s1)
    | (.ok _, s1) =>
      if s1.evalDepth ≥ MAX_EVAL_DEPTH then
        (.error .evalDepthExceeded, s1)
      else
        let s1' := { s1 with evalDepth := s1.evalDepth + 1 }
        let (outcome, sEnd) : Except Err Val × EvalSt :=
          match e with
          | .lit v => (.ok v, s1')
          | .var sym =>
            -- get_local first; a global that is absent *or Null* is VariableNotFound
            -- (the `Expr::Variable` arm of `types.rs::StateMachine::evaluate_impl`
            -- — the Null-global quirk is mirrored, FIDELITY §S3).
            match alookup env sym with
            | some v => (.ok v, s1')
            | none =>
              match alookup σ sym with
              | none => (.error (.varNotFound sym), s1')
              | some v =>
                match v with
                | .null => (.error (.varNotFound sym), s1')
                | _ => (.ok v, s1')
          | .binop op l r =>
            match evalE B D σ fuel' env l s1' with
            | (.error er, s2) => (.error er, s2)
            | (.ok lv, s2) =>
              match evalE B D σ fuel' env r s2 with
              | (.error er, s3) => (.error er, s3)
              | (.ok rv, s3) =>
                -- `binopCost` is `0` for every operator/operand pair except
                -- string `Add`, which charges `l.length + r.length` before
                -- `applyBinop` runs (`logic.rs::apply_binop`) — the same
                -- pre-charge-before-native-work discipline as `evalFilter`/
                -- `evalCount`/`evalSort`.
                match charge B (binopCost op lv rv) s3 with
                | (.error er, s4) => (.error er, s4)
                | (.ok _, s4) =>
                  match applyBinop op lv rv with
                  | .ok v => (.ok v, s4)
                  | .error er => (.error er, s4)
          | .not inner =>
            match evalE B D σ fuel' env inner s1' with
            | (.error er, s2) => (.error er, s2)
            | (.ok v, s2) =>
              match v with
              | .bool b => (.ok (.bool !b), s2)
              | _ => (.error .typeError, s2)
          | .ite c t el =>
            match evalE B D σ fuel' env c s1' with
            | (.error er, s2) => (.error er, s2)
            | (.ok v, s2) =>
              match v with
              | .bool true => evalE B D σ fuel' env t s2
              | .bool false => evalE B D σ fuel' env el s2
              | _ => (.error .typeError, s2)
          | .field base f =>
            match evalE B D σ fuel' env base s1' with
            | (.error er, s2) => (.error er, s2)
            | (.ok v, s2) =>
              match v with
              | .record fs =>
                match alookup fs f with
                | some fv => (.ok fv, s2)
                | none => (.error (.execError "field not found"), s2)
              | _ => (.error .typeError, s2)
          | .letE name v body =>
            match evalE B D σ fuel' env v s1' with
            | (.error er, s2) => (.error er, s2)
            | (.ok bv, s2) => evalE B D σ fuel' ((name, bv) :: env) body s2
          | .call fname args => evalCall B D σ fuel' env fname args s1'
        (outcome, { sEnd with evalDepth := sEnd.evalDepth - 1 })
  termination_by (fuel, 0, 0)

/-- store_local(key, value): key evaluated and string-checked, then value
evaluated, then the write-only storage effect is queued (the `"store_local"`
arm of `types.rs::StateMachine::evaluate_impl`). -/
def evalStoreLocal (B : Nat) (D : Doc) (σ : Store) (fuel : Nat) (env : Env)
    (k v : Expr) (s : EvalSt) : Res Val :=
  match evalE B D σ fuel env k s with
  | (.error er, s1) => (.error er, s1)
  | (.ok kv, s1) =>
    match kv with
    | .str ks =>
      match evalE B D σ fuel env v s1 with
      | (.error er, s2) => (.error er, s2)
      | (.ok vv, s2) => (.ok (.bool true), emit (.storeLocal ks vv) s2)
    | _ => (.error (.execError "store_local key must be a string"), s1)
  termination_by (fuel, 2, 0)

/-- copy_to_clipboard(node_id) (the `"copy_to_clipboard"` arm of
`types.rs::StateMachine::evaluate_impl`). -/
def evalClip (B : Nat) (D : Doc) (σ : Store) (fuel : Nat) (env : Env)
    (x : Expr) (s : EvalSt) : Res Val :=
  match evalE B D σ fuel env x s with
  | (.error er, s1) => (.error er, s1)
  | (.ok v, s1) =>
    match v with
    | .str nodeId => (.ok (.bool true), emit (.copyClipboard nodeId) s1)
    | _ => (.error (.execError "copy_to_clipboard argument must be a node id string"), s1)
  termination_by (fuel, 2, 0)

/-- filter(list, field, target): all three arguments evaluated, list
type-checked, then the budget is pre-charged with `len` *before* the native
O(n) pass (the `"filter"` arm of `types.rs::StateMachine::evaluate_impl`). -/
def evalFilter (B : Nat) (D : Doc) (σ : Store) (fuel : Nat) (env : Env)
    (l f t : Expr) (s : EvalSt) : Res Val :=
  match evalE B D σ fuel env l s with
  | (.error er, s1) => (.error er, s1)
  | (.ok lv, s1) =>
    match evalE B D σ fuel env f s1 with
    | (.error er, s2) => (.error er, s2)
    | (.ok fv, s2) =>
      match evalE B D σ fuel env t s2 with
      | (.error er, s3) => (.error er, s3)
      | (.ok tv, s3) =>
        match lv with
        | .list xs =>
          match charge B xs.length s3 with
          | (.error er, s4) => (.error er, s4)
          | (.ok _, s4) =>
            match fv with
            | .str fs => (.ok (.list (filterField xs fs tv)), s4)
            | _ => (.error .typeError, s4)
        | _ => (.error .typeError, s3)
  termination_by (fuel, 2, 0)

/-- count(list, field, target) (the `"count"` arm of
`types.rs::StateMachine::evaluate_impl`). -/
def evalCount (B : Nat) (D : Doc) (σ : Store) (fuel : Nat) (env : Env)
    (l f t : Expr) (s : EvalSt) : Res Val :=
  match evalE B D σ fuel env l s with
  | (.error er, s1) => (.error er, s1)
  | (.ok lv, s1) =>
    match evalE B D σ fuel env f s1 with
    | (.error er, s2) => (.error er, s2)
    | (.ok fv, s2) =>
      match evalE B D σ fuel env t s2 with
      | (.error er, s3) => (.error er, s3)
      | (.ok tv, s3) =>
        match lv with
        | .list xs =>
          match charge B xs.length s3 with
          | (.error er, s4) => (.error er, s4)
          | (.ok _, s4) =>
            match fv with
            | .str fs => (.ok (.int (countField xs fs tv)), s4)
            | _ => (.error .typeError, s4)
        | _ => (.error .typeError, s3)
  termination_by (fuel, 2, 0)

/-- sort(list, field, direction): pre-charge `n·(⌊log₂ n⌋+1)` (the `"sort"`
arm of `types.rs::StateMachine::evaluate_impl`).  The bare-identifier
asc/desc sugar is not modeled (FIDELITY §B4): direction must evaluate to a
string. -/
def evalSort (B : Nat) (D : Doc) (σ : Store) (fuel : Nat) (env : Env)
    (l f dir : Expr) (s : EvalSt) : Res Val :=
  match evalE B D σ fuel env l s with
  | (.error er, s1) => (.error er, s1)
  | (.ok lv, s1) =>
    match evalE B D σ fuel env f s1 with
    | (.error er, s2) => (.error er, s2)
    | (.ok fv, s2) =>
      match evalE B D σ fuel env dir s2 with
      | (.error er, s3) => (.error er, s3)
      | (.ok dv, s3) =>
        match lv with
        | .list xs =>
          match charge B (sortCost xs.length) s3 with
          | (.error er, s4) => (.error er, s4)
          | (.ok _, s4) =>
            match fv with
            | .str fs =>
              match dv with
              | .str d =>
                if d == "asc" || d == "desc" then
                  (.ok (.list (sortByField fs (d == "desc") xs)), s4)
                else (.error (.execError "sort: direction must be `asc` or `desc`"), s4)
              | _ => (.error .typeError, s4)
            | _ => (.error .typeError, s4)
        | _ => (.error .typeError, s3)
  termination_by (fuel, 2, 0)

/-- Builtin dispatch for `Expr::FunctionCall` — mirrors the name-keyed match
in `evaluate_impl` (`types.rs::StateMachine::evaluate_impl`).
`filter`/`count`/`sort`/`download`
fall through to user functions on arity mismatch (Rust arm guards);
`store_local`/`copy_to_clipboard`/`get_system_time` error out (no guard). -/
def evalCall (B : Nat) (D : Doc) (σ : Store) (fuel : Nat) (env : Env)
    (fname : Symbol) (args : List Expr) (s : EvalSt) : Res Val :=
  match D.builtinOf fname with
  | some .storeLocal =>
    match args with
    | [k, v] => evalStoreLocal B D σ fuel env k v s
    | _ => (.error (.execError "store_local expects 2 arguments"), s)
  | some .copyClipboard =>
    match args with
    | [x] => evalClip B D σ fuel env x s
    | _ => (.error (.execError "copy_to_clipboard expects 1 argument"), s)
  | some .download =>
    match args with
    | [a] => evalDownload a s
    | _ => evalUser B D σ fuel env fname args s
  | some .getSystemTime =>
    match args with
    | [a] => evalGetSystemTime D a s
    | _ => (.error (.execError "get_system_time expects 1 argument"), s)
  | some .filter =>
    match args with
    | [l, f, t] => evalFilter B D σ fuel env l f t s
    | _ => evalUser B D σ fuel env fname args s
  | some .count =>
    match args with
    | [l, f, t] => evalCount B D σ fuel env l f t s
    | _ => evalUser B D σ fuel env fname args s
  | some .sort =>
    match args with
    | [l, f, d] => evalSort B D σ fuel env l f d s
    | _ => evalUser B D σ fuel env fname args s
  | none => evalUser B D σ fuel env fname args s
  termination_by (fuel, 3, 0)

/-- User-defined function call (the user-function fallback at the end of
`types.rs::StateMachine::evaluate_impl`): args evaluated left-to-right in
the *caller's* env, then the body runs in a fresh frame containing exactly
the parameters (frame_pointer semantics). -/
def evalUser (B : Nat) (D : Doc) (σ : Store) (fuel : Nat) (env : Env)
    (fname : Symbol) (args : List Expr) (s : EvalSt) : Res Val :=
  match alookup D.functions fname with
  | none => (.error (.execError "call to undefined function"), s)
  | some fd =>
    if args.length == fd.params.length then
      match evalArgs B D σ fuel env args s with
      | (.error er, s1) => (.error er, s1)
      | (.ok vals, s1) => evalE B D σ fuel (bindParams (fd.params.map Prod.fst) vals) fd.body s1
    else (.error (.execError "function arity mismatch"), s)
  termination_by (fuel, 2, 0)

/-- Left-to-right evaluation of an argument list (the `for arg_expr in args`
loop in the user-function fallback of `types.rs::StateMachine::evaluate_impl`).
The loop itself is not a Rust-level `evaluate`
call, so it consumes no fuel; each element does. -/
def evalArgs (B : Nat) (D : Doc) (σ : Store) (fuel : Nat) (env : Env)
    (args : List Expr) (s : EvalSt) : Res (List Val) :=
  match args with
  | [] => (.ok [], s)
  | a :: rest =>
    match evalE B D σ fuel env a s with
    | (.error er, s1) => (.error er, s1)
    | (.ok v, s1) =>
      match evalArgs B D σ fuel env rest s1 with
      | (.error er, s2) => (.error er, s2)
      | (.ok vs, s2) => (.ok (v :: vs), s2)
  termination_by (fuel, 1, args.length)

end

/-! ## Actions -/

/-- Result of `execute_action`. -/
structure ActRes where
  err     : Option Err
  σ       : Store
  effects : List Effect
  work    : Nat
  changed : List Symbol
  deriving Repr

/-- Second half of a `NetworkCall`: evaluate the optional `path_param` under
the *same* instruction counter (no reset between payload and path), apply the
A1 type check (string or number) and the A2 single-segment gate (G2) via
`pathParamOk`, then queue the call (the `Action::NetworkCall` arm of
`parser::logic::execute_action`, `logic.rs::execute_action`). -/
def execNetPath (B : Nat) (D : Doc) (σ : Store) (m : Method) (alias : Symbol)
    (pv : Option Val) (pathp : Option Expr) (target : Symbol) (s : EvalSt) : ActRes :=
  match pathp with
  | none => ⟨none, σ, s.effects ++ [.networkCall m alias none pv target], s.work, []⟩
  | some pp =>
    match evalE B D σ (B + 2) [] pp s with
    | (.error er, s') => ⟨some er, σ, s'.effects, s'.work, []⟩
    | (.ok v, s') =>
      match v with
      | .str str =>
        if pathParamOk str then
          ⟨none, σ, s'.effects ++ [.networkCall m alias (some str) pv target], s'.work, []⟩
        else ⟨some (.execError "path_param must be a single path segment"), σ, s'.effects, s'.work, []⟩
      | .int n =>
        if pathParamOk (toString n) then
          ⟨none, σ, s'.effects ++ [.networkCall m alias (some (toString n)) pv target], s'.work, []⟩
        else ⟨some (.execError "path_param must be a single path segment"), σ, s'.effects, s'.work, []⟩
      | _ => ⟨some (.execError "path_param must be a string or number"), σ, s'.effects, s'.work, []⟩

/-- Mirrors `execute_action` (`logic.rs::execute_action`).  The instruction counter
is reset per action (fresh `EvalSt.init`); the fuel is `B + 2`, which
`Budget.lean` proves sufficient.  The store is only written by `assign`
(and only on success). -/
def execAction (B : Nat) (D : Doc) (σ : Store) (a : Action) : ActRes :=
  match a with
  | .eval e =>
    match evalE B D σ (B + 2) [] e .init with
    | (.ok _, s) => ⟨none, σ, s.effects, s.work, []⟩
    | (.error er, s) => ⟨some er, σ, s.effects, s.work, []⟩
  | .assign t e =>
    -- assignment to computed variables is rejected (the `Action::Assign`
    -- arm of `logic.rs::execute_action`)
    if D.compSyms.contains t then
      ⟨some (.execError "cannot assign to computed variable"), σ, [], 0, []⟩
    else
      match evalE B D σ (B + 2) [] e .init with
      | (.ok v, s) => ⟨none, setVar σ t v, s.effects, s.work, [t]⟩
      | (.error er, s) => ⟨some er, σ, s.effects, s.work, []⟩
  | .navigate url =>
    match evalE B D σ (B + 2) [] url .init with
    | (.ok (.str u), s) => ⟨none, σ, s.effects ++ [.navigate u], s.work, []⟩
    | (.ok _, s) => ⟨some (.execError "Navigate URL must evaluate to a string"), σ, s.effects, s.work, []⟩
    | (.error er, s) => ⟨some er, σ, s.effects, s.work, []⟩
  | .networkCall m alias payload pathp target =>
    -- payload then path_param, same instruction counter (no reset between).
    match payload with
    | none => execNetPath B D σ m alias none pathp target EvalSt.init
    | some p =>
      match evalE B D σ (B + 2) [] p .init with
      | (.error er, s) => ⟨some er, σ, s.effects, s.work, []⟩
      | (.ok v, s) => execNetPath B D σ m alias (some v) pathp target s

/-! ## Static dependency collection (mirrors `collect_vars` +
`parse_computed_with_functions`' transitive reads) -/

mutual
def collectVars : Expr → List Symbol
  | .lit _ => []
  | .var s => [s]
  | .binop _ l r => collectVars l ++ collectVars r
  | .call _ args => collectVarsL args
  | .letE _ v b => collectVars v ++ collectVars b
  | .not i => collectVars i
  | .ite c t e => collectVars c ++ collectVars t ++ collectVars e
  | .field b _ => collectVars b
def collectVarsL : List Expr → List Symbol
  | [] => []
  | a :: as => collectVars a ++ collectVarsL as
end

mutual
def collectCalls : Expr → List Symbol
  | .lit _ => []
  | .var _ => []
  | .binop _ l r => collectCalls l ++ collectCalls r
  | .call f args => f :: collectCallsL args
  | .letE _ v b => collectCalls v ++ collectCalls b
  | .not i => collectCalls i
  | .ite c t e => collectCalls c ++ collectCalls t ++ collectCalls e
  | .field b _ => collectCalls b
def collectCallsL : List Expr → List Symbol
  | [] => []
  | a :: as => collectCalls a ++ collectCallsL as
end

/-- Transitive read set of an expression through called functions, cut off at
`fuel` unfoldings.  `compDeps` instantiates fuel at `|functions| + 1`, which
covers every call chain of a DAG-checked document. -/
def collectReads (D : Doc) : Nat → Expr → List Symbol
  | 0, e => collectVars e
  | fuel + 1, e =>
    collectVars e ++ (collectCalls e).flatMap fun g =>
      match alookup D.functions g with
      | some fd => collectReads D fuel fd.body
      | none => []

/-- Static dependency set of a computed binding (mirrors
`ComputedBinding::depends_on` as derived by `parse_computed_with_functions`). -/
def compDeps (D : Doc) (c : CompDef) : List Symbol :=
  collectReads D (D.functions.length + 1) c.expr

/-! ## Computed-variable recomputation -/

structure RecompRes where
  σ       : Store
  effects : List Effect
  work    : Nat
  changed : List Symbol
  errs    : List Err
  deriving Repr

/-- One binding of `recompute_computed_bindings`
(`logic.rs::recompute_computed_bindings`): trigger if any static dep
changed; fresh instruction budget per comp;
on error the value write is skipped but queued effects survive (the Rust
loop never truncates `accumulated_actions`). -/
def recomputeStep (B : Nat) (D : Doc) (acc : RecompRes) (c : CompDef) : RecompRes :=
  if (compDeps D c).any (fun d => acc.changed.contains d) then
    match evalE B D acc.σ (B + 2) [] c.expr .init with
    | (.ok v, s) =>
      ⟨setVar acc.σ c.name v, acc.effects ++ s.effects, acc.work + s.work,
       c.name :: acc.changed, acc.errs⟩
    | (.error er, s) =>
      ⟨acc.σ, acc.effects ++ s.effects, acc.work + s.work, acc.changed, acc.errs ++ [er]⟩
  else acc

def recompute (B : Nat) (D : Doc) (σ : Store) (changed : List Symbol) : RecompRes :=
  D.comps.foldl (recomputeStep B D) ⟨σ, [], 0, changed, []⟩

/-! ## Reactions -/

def tag (c : Ctx) (effs : List Effect) : List (Ctx × Effect) := effs.map ((c, ·))

/-- Result of one reaction. -/
structure ReactRes where
  σ     : Store
  trace : List (Ctx × Effect)
  work  : Nat

/-- Transactional action firing — mirrors `execute_and_respond`
(`logic_worker.rs::LogicWorker::execute_and_respond`): on error the store is
rolled back (undo log) and queued effects are truncated; recomputation runs
only on success. -/
def fireTx (B : Nat) (D : Doc) (σ : Store) (c : Ctx) (a : Action) : ReactRes :=
  match execAction B D σ a with
  | ⟨some _, _, _, w, _⟩ => ⟨σ, [], w⟩
  | ⟨none, σ', effs, w, ch⟩ =>
    ⟨(recompute B D σ' ch).σ, tag c (effs ++ (recompute B D σ' ch).effects),
     w + (recompute B D σ' ch).work⟩

/-- Submit-path action firing (the `UiEvent::SubmitForm` arm of
`logic_worker.rs::LogicWorker::run_loop` — submit does not route through
`execute_and_respond` the way click/timer do): the `$form` write (already
applied in `σ0`) survives action failure, and — unlike the click/timer path
— queued effects are *not* truncated on error. -/
def fireSubmit (B : Nat) (D : Doc) (σ0 : Store) (formSym : Symbol) (a : Action) : ReactRes :=
  match execAction B D σ0 a with
  | ⟨some _, _, effs, w, _⟩ =>
    ⟨(recompute B D σ0 [formSym]).σ,
     tag .gesture (effs ++ (recompute B D σ0 [formSym]).effects),
     w + (recompute B D σ0 [formSym]).work⟩
  | ⟨none, σ', effs, w, ch⟩ =>
    ⟨(recompute B D σ' (formSym :: ch)).σ,
     tag .gesture (effs ++ (recompute B D σ' (formSym :: ch)).effects),
     w + (recompute B D σ' (formSym :: ch)).work⟩

/-- One reaction to one input event — mirrors `LogicWorker::run_loop`
(`logic_worker.rs::LogicWorker::run_loop`).

* `click`/`timer`: transactional (`execute_and_respond`,
  `logic_worker.rs::LogicWorker::execute_and_respond`).
* `submit`: `$form` is populated first and survives action failure; the
  submit path does **not** truncate effects on error (the `UiEvent::SubmitForm`
  arm of `run_loop`). Modeled in **form-record semantics**: the per-field
  mirror-writes into same-named declared globals (the `for (field_name,
  field_value) in fields` loop in that same `UiEvent::SubmitForm` arm,
  calling `types.rs::VariableStore::set_runtime` per field) are *not*
  modeled — this is a deliberate, documented divergence (Finding 1 in
  `RESULTS.md`).
* `netResponse`: `UiEvent::UpdateVariable` — declared-only write (frozen
  interner), then recomputation. -/
def reaction (B : Nat) (D : Doc) (σ : Store) : Event → ReactRes
  | .click i =>
    match D.clicks[i]? with
    | none => ⟨σ, [], 0⟩
    | some a => fireTx B D σ .gesture a
  | .timer i =>
    match D.timers[i]? with
    | none => ⟨σ, [], 0⟩
    | some a => fireTx B D σ .nonInteractive a
  | .submit i fields =>
    match D.submits[i]? with
    | none =>
      ⟨(recompute B D (setVar σ D.formSym (.record fields)) [D.formSym]).σ,
       tag .gesture (recompute B D (setVar σ D.formSym (.record fields)) [D.formSym]).effects,
       (recompute B D (setVar σ D.formSym (.record fields)) [D.formSym]).work⟩
    | some a => fireSubmit B D (setVar σ D.formSym (.record fields)) D.formSym a
  | .netResponse t v =>
    if D.declared.contains t then
      ⟨(recompute B D (setVar σ t v) [t]).σ,
       tag .nonInteractive (recompute B D (setVar σ t v) [t]).effects,
       (recompute B D (setVar σ t v) [t]).work⟩
    else ⟨σ, [], 0⟩

/-- A run: fold reactions over an input event list, concatenating traces. -/
def run (B : Nat) (D : Doc) : Store → List Event → Store × List (Ctx × Effect)
  | σ, [] => (σ, [])
  | σ, ev :: evs =>
    ((run B D (reaction B D σ ev).σ evs).1,
     (reaction B D σ ev).trace ++ (run B D (reaction B D σ ev).σ evs).2)

/-! ## Trace projections -/

/-- The destinations reached **without a gate**: navigation requests emitted
from a non-interactive context (root timer).  Gesture-context navigations are
discharged by gate G1; `networkCall` destinations are static registry aliases
with the G2-validated path — neither appears here. -/
def ungatedNavs (tr : List (Ctx × Effect)) : List String :=
  tr.filterMap fun ce =>
    match ce with
    | (.nonInteractive, .navigate u) => some u
    | _ => none

/-- All network-call aliases in a trace (for the registry-confinement
corollary). -/
def netAliases (tr : List (Ctx × Effect)) : List Symbol :=
  tr.filterMap fun ce =>
    match ce with
    | (_, .networkCall _ alias _ _ _) => some alias
    | _ => none

end Mizu
