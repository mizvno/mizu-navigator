import MizuFormal.Semantics

/-!
# The load-time information-flow checker

Mirrors `parser::flow::check_information_flow` (`src/parser/flow.rs`), the
enforcement of invariant **F1** in `SECURITY-INVARIANTS.md`.

Citations below name the Rust item (`file.rs::item`) rather than a line
number — see the citation convention note at the end of `Syntax.lean`.

* `isTaintedE` mirrors `is_expr_tainted` clause by clause
  (`flow.rs::is_expr_tainted`).
* Sources: the `$form` symbol and every `NetworkCall.target_var` (the
  "Initialize tainted sources" block of
  `flow.rs::check_information_flow`).
* Propagation: functions whose body is tainted, comps whose RHS is tainted,
  `Assign` targets of tainted expressions (the "Propagation (fixpoint)"
  block of `flow.rs::check_information_flow`).
* Sinks: `Action::Navigate.url`; gate G1 = user-gesture context (the "Check
  sinks" block of `flow.rs::check_information_flow`).  `path_param` is
  **not** a sink — gate G2 is unconditional at runtime (`execNetPath`).

## Divergence from `flow.rs` (documented in `FIDELITY.md` §F2)

`flow.rs` iterates `while changed` to the least fixpoint.  The model iterates
a syntactic bound and then **checks stability** (`Taint.stable`); an
unconverged analysis is rejected.  This makes soundness independent of any
convergence argument (fail-secure — precisely the checker's own philosophy),
at zero cost in precision for real documents: the bound
`#functions + #comps + #actions + 1` dominates the number of productive
iterations of a monotone pass exactly as in the Rust loop.
-/

namespace Mizu

/-- The analysis state: tainted variables and tainted functions. -/
structure Taint where
  vars : List Symbol
  fns  : List Symbol
  deriving Repr

mutual
/-- Mirrors `is_expr_tainted` (`flow.rs::is_expr_tainted`): a call is tainted
iff the callee is tainted or any argument is; everything else propagates
structurally. -/
def isTaintedE (T : Taint) : Expr → Bool
  | .lit _ => false
  | .var s => T.vars.contains s
  | .binop _ l r => isTaintedE T l || isTaintedE T r
  | .call f args => T.fns.contains f || isTaintedL T args
  | .letE _ v b => isTaintedE T v || isTaintedE T b
  | .not i => isTaintedE T i
  | .ite c t e => isTaintedE T c || isTaintedE T t || isTaintedE T e
  | .field b _ => isTaintedE T b
def isTaintedL (T : Taint) : List Expr → Bool
  | [] => false
  | a :: as => isTaintedE T a || isTaintedL T as
end

/-- All handler actions with their gate context — mirrors the action
collection loop at the top of `flow.rs::check_information_flow`:
click/submit events are `UserGesture`; root timers are `NonInteractive`. -/
def allActions (D : Doc) : List (Ctx × Action) :=
  D.clicks.map (fun a => (Ctx.gesture, a))
    ++ D.submits.map (fun a => (Ctx.gesture, a))
    ++ D.timers.map (fun a => (Ctx.nonInteractive, a))

/-- Every `NetworkCall.target_var` — tainted from load (the `NetworkCall`
loop in the "Initialize tainted sources" block of
`flow.rs::check_information_flow`). -/
def netTargets (D : Doc) : List Symbol :=
  (allActions D).filterMap fun ca =>
    match ca.2 with
    | .networkCall _ _ _ _ t => some t
    | _ => none

/-- Taint sources: `$form` and network-response targets. -/
def sources (D : Doc) : List Symbol := D.formSym :: netTargets D

/-- One (parallel) propagation pass — inflationary by construction. -/
def stepTaint (D : Doc) (T : Taint) : Taint :=
  { vars := T.vars
      ++ (D.comps.filterMap fun c =>
           if !T.vars.contains c.name && isTaintedE T c.expr then some c.name else none)
      ++ ((allActions D).filterMap fun ca =>
           match ca.2 with
           | .assign t e => if !T.vars.contains t && isTaintedE T e then some t else none
           | _ => none)
    fns := T.fns
      ++ D.functions.filterMap fun fd =>
           if !T.fns.contains fd.1 && isTaintedE T fd.2.body then some fd.1 else none }

def iterTaint (D : Doc) : Nat → Taint → Taint
  | 0, T => T
  | k + 1, T => iterTaint D k (stepTaint D T)

/-- Iteration bound: enough for any monotone pass over this document. -/
def taintBound (D : Doc) : Nat :=
  D.functions.length + D.comps.length + (allActions D).length + 1

/-- The taint sets the checker computes. -/
def taintOf (D : Doc) : Taint :=
  iterTaint D (taintBound D) ⟨sources D, []⟩

/-- Post-fixpoint check: one more pass adds nothing. -/
def Taint.stable (D : Doc) (T : Taint) : Bool :=
  ((stepTaint D T).vars.all fun s => T.vars.contains s)
    && ((stepTaint D T).fns.all fun f => T.fns.contains f)

/-- Sink check (the "Check sinks" block of `flow.rs::check_information_flow`):
a tainted `navigate` URL is only acceptable under gate G1 (user gesture). -/
def sinkCheck (D : Doc) (T : Taint) : Bool :=
  (allActions D).all fun ca =>
    match ca with
    | (ctx, .navigate url) => ctx == Ctx.gesture || !isTaintedE T url
    | _ => true

/-- **The flow checker** — the model's `check_information_flow`. -/
def accept (D : Doc) : Bool :=
  Taint.stable D (taintOf D) && sinkCheck D (taintOf D)

/-! ## Extraction lemmas -/

theorem contains_iff_mem {l : List Symbol} {s : Symbol} :
    l.contains s = true ↔ s ∈ l := by
  simp

theorem contains_false_iff {l : List Symbol} {s : Symbol} :
    l.contains s = false ↔ s ∉ l := by
  constructor
  · intro h hm
    rw [contains_iff_mem.mpr hm] at h
    cases h
  · intro h
    cases hc : l.contains s
    · rfl
    · exact absurd (contains_iff_mem.mp hc) h

theorem alookup_mem {l : List (Symbol × α)} {k : Symbol} {v : α}
    (h : alookup l k = some v) : (k, v) ∈ l := by
  induction l with
  | nil => cases h
  | cons p rest ih =>
    rw [alookup] at h
    split at h
    · rename_i hbeq
      injection h with h2
      obtain ⟨k', v'⟩ := p
      simp at hbeq
      subst hbeq
      subst h2
      exact List.mem_cons_self ..
    · exact List.mem_cons_of_mem _ (ih h)

theorem accept_parts {D : Doc} (h : accept D = true) :
    Taint.stable D (taintOf D) = true ∧ sinkCheck D (taintOf D) = true := by
  have h2 : (Taint.stable D (taintOf D) && sinkCheck D (taintOf D)) = true := h
  simp only [Bool.and_eq_true] at h2
  exact h2

theorem accept_stable {D : Doc} (h : accept D = true) :
    Taint.stable D (taintOf D) = true := (accept_parts h).1

theorem accept_sinks {D : Doc} (h : accept D = true) :
    sinkCheck D (taintOf D) = true := (accept_parts h).2

/-- The two halves of stability, in membership form. -/
theorem stable_parts {D : Doc} {T : Taint} (hst : Taint.stable D T = true) :
    (∀ s ∈ (stepTaint D T).vars, T.vars.contains s = true)
    ∧ (∀ f ∈ (stepTaint D T).fns, T.fns.contains f = true) := by
  have h2 : (((stepTaint D T).vars.all fun s => T.vars.contains s)
      && ((stepTaint D T).fns.all fun f => T.fns.contains f)) = true := hst
  simp only [Bool.and_eq_true, List.all_eq_true] at h2
  exact h2

/-- Sources stay tainted through iteration (`stepTaint` is inflationary). -/
theorem stepTaint_vars_sub {D : Doc} {T : Taint} {s : Symbol}
    (h : s ∈ T.vars) : s ∈ (stepTaint D T).vars := by
  unfold stepTaint
  exact List.mem_append_left _ (List.mem_append_left _ h)

theorem iterTaint_vars_sub {D : Doc} :
    ∀ (k : Nat) (T : Taint) {s : Symbol}, s ∈ T.vars → s ∈ (iterTaint D k T).vars := by
  intro k
  induction k with
  | zero => intro T s h; exact h
  | succ n ih =>
    intro T s h
    exact ih _ (stepTaint_vars_sub h)

theorem sources_tainted {D : Doc} {s : Symbol} (h : s ∈ sources D) :
    (taintOf D).vars.contains s = true := by
  unfold taintOf
  exact contains_iff_mem.mpr (iterTaint_vars_sub _ _ h)

theorem formSym_tainted (D : Doc) : (taintOf D).vars.contains D.formSym = true :=
  sources_tainted (List.mem_cons_self ..)

theorem netTarget_tainted {D : Doc} {t : Symbol} (h : t ∈ netTargets D) :
    (taintOf D).vars.contains t = true :=
  sources_tainted (List.mem_cons_of_mem _ h)

/-! ### Closure facts from stability

`accept` verifies that one more pass adds nothing; contrapositively, every
propagation rule is already saturated. -/

/-- Function closure: an untainted function has an untainted body. -/
theorem stable_fn {D : Doc} {T : Taint} (hst : Taint.stable D T = true)
    {f : Symbol} {fd : FunDef} (hmem : (f, fd) ∈ D.functions)
    (hnf : T.fns.contains f = false) : isTaintedE T fd.body = false := by
  cases hb : isTaintedE T fd.body
  · rfl
  · exfalso
    have harm : (if !T.fns.contains f && isTaintedE T fd.body then some f else none)
        = some f := by
      rw [hnf, hb]; rfl
    have hstep : f ∈ (stepTaint D T).fns :=
      List.mem_append_right _ (List.mem_filterMap.mpr ⟨(f, fd), hmem, harm⟩)
    have := (stable_parts hst).2 f hstep
    rw [hnf] at this
    cases this

/-- Comp closure: an untainted comp has an untainted RHS. -/
theorem stable_comp {D : Doc} {T : Taint} (hst : Taint.stable D T = true)
    {c : CompDef} (hmem : c ∈ D.comps)
    (hnc : T.vars.contains c.name = false) : isTaintedE T c.expr = false := by
  cases hb : isTaintedE T c.expr
  · rfl
  · exfalso
    have harm : (if !T.vars.contains c.name && isTaintedE T c.expr then some c.name else none)
        = some c.name := by
      rw [hnc, hb]; rfl
    have hstep : c.name ∈ (stepTaint D T).vars :=
      List.mem_append_left _ (List.mem_append_right _
        (List.mem_filterMap.mpr ⟨c, hmem, harm⟩))
    have := (stable_parts hst).1 c.name hstep
    rw [hnc] at this
    cases this

/-- Assign closure: an assignment to an untainted target has an untainted
RHS. -/
theorem stable_assign {D : Doc} {T : Taint} (hst : Taint.stable D T = true)
    {ctx : Ctx} {t : Symbol} {e : Expr} (hmem : (ctx, Action.assign t e) ∈ allActions D)
    (hnt : T.vars.contains t = false) : isTaintedE T e = false := by
  cases hb : isTaintedE T e
  · rfl
  · exfalso
    have harm : (match (ctx, Action.assign t e).2 with
        | Action.assign t e =>
          if !T.vars.contains t && isTaintedE T e then some t else none
        | _ => none) = some t := by
      show (if !T.vars.contains t && isTaintedE T e then some t else none) = some t
      rw [hnt, hb]; rfl
    have hstep : t ∈ (stepTaint D T).vars :=
      List.mem_append_right _
        (List.mem_filterMap.mpr ⟨(ctx, Action.assign t e), hmem, harm⟩)
    have := (stable_parts hst).1 t hstep
    rw [hnt] at this
    cases this

/-- Sink fact: a non-interactive `navigate` in an accepted document has an
untainted URL expression. -/
theorem accept_navigate {D : Doc} (hacc : accept D = true)
    {url : Expr} (hmem : (Ctx.nonInteractive, Action.navigate url) ∈ allActions D) :
    isTaintedE (taintOf D) url = false := by
  have hall := accept_sinks hacc
  rw [sinkCheck, List.all_eq_true] at hall
  have := hall _ hmem
  simp at this
  rcases this with h | h
  · exact absurd h (by decide)
  · exact h

/-- The function-closure hypothesis used throughout the agreement proofs,
phrased on `alookup` (the form the evaluator uses). -/
def FnClosure (D : Doc) (T : Taint) : Prop :=
  ∀ f fd, alookup D.functions f = some fd → T.fns.contains f = false →
    isTaintedE T fd.body = false

theorem accept_fnClosure {D : Doc} (hacc : accept D = true) :
    FnClosure D (taintOf D) := by
  intro f fd hlk hnf
  exact stable_fn (accept_stable hacc) (alookup_mem hlk) hnf

/-! ### Untaintedness pushes to collected variables and calls -/

mutual
theorem collectVars_unt (T : Taint) :
    ∀ (e : Expr), isTaintedE T e = false →
      ∀ s ∈ collectVars e, T.vars.contains s = false
  | .lit v => by
    intro _ s hs
    simp [collectVars] at hs
  | .var x => by
    intro h s hs
    simp [collectVars] at hs
    subst hs
    simpa [isTaintedE] using h
  | .binop op l r => by
    intro h s hs
    simp only [isTaintedE, Bool.or_eq_false_iff] at h
    simp only [collectVars, List.mem_append] at hs
    rcases hs with hs | hs
    · exact collectVars_unt T l h.1 s hs
    · exact collectVars_unt T r h.2 s hs
  | .call f args => by
    intro h s hs
    simp only [isTaintedE, Bool.or_eq_false_iff] at h
    simp only [collectVars] at hs
    exact collectVarsL_unt T args h.2 s hs
  | .letE n v b => by
    intro h s hs
    simp only [isTaintedE, Bool.or_eq_false_iff] at h
    simp only [collectVars, List.mem_append] at hs
    rcases hs with hs | hs
    · exact collectVars_unt T v h.1 s hs
    · exact collectVars_unt T b h.2 s hs
  | .not i => by
    intro h s hs
    simp only [isTaintedE] at h
    simp only [collectVars] at hs
    exact collectVars_unt T i h s hs
  | .ite c t e => by
    intro h s hs
    simp only [isTaintedE, Bool.or_eq_false_iff] at h
    simp only [collectVars, List.mem_append] at hs
    rcases hs with (hs | hs) | hs
    · exact collectVars_unt T c h.1.1 s hs
    · exact collectVars_unt T t h.1.2 s hs
    · exact collectVars_unt T e h.2 s hs
  | .field b f => by
    intro h s hs
    simp only [isTaintedE] at h
    simp only [collectVars] at hs
    exact collectVars_unt T b h s hs

theorem collectVarsL_unt (T : Taint) :
    ∀ (es : List Expr), isTaintedL T es = false →
      ∀ s ∈ collectVarsL es, T.vars.contains s = false
  | [] => by
    intro _ s hs
    simp [collectVarsL] at hs
  | a :: as => by
    intro h s hs
    simp only [isTaintedL, Bool.or_eq_false_iff] at h
    simp only [collectVarsL, List.mem_append] at hs
    rcases hs with hs | hs
    · exact collectVars_unt T a h.1 s hs
    · exact collectVarsL_unt T as h.2 s hs
end

mutual
theorem collectCalls_unt (T : Taint) :
    ∀ (e : Expr), isTaintedE T e = false →
      ∀ g ∈ collectCalls e, T.fns.contains g = false
  | .lit v => by
    intro _ g hg
    simp [collectCalls] at hg
  | .var x => by
    intro _ g hg
    simp [collectCalls] at hg
  | .binop op l r => by
    intro h g hg
    simp only [isTaintedE, Bool.or_eq_false_iff] at h
    simp only [collectCalls, List.mem_append] at hg
    rcases hg with hg | hg
    · exact collectCalls_unt T l h.1 g hg
    · exact collectCalls_unt T r h.2 g hg
  | .call f args => by
    intro h g hg
    simp only [isTaintedE, Bool.or_eq_false_iff] at h
    simp only [collectCalls, List.mem_cons] at hg
    rcases hg with hg | hg
    · subst hg
      exact h.1
    · exact collectCallsL_unt T args h.2 g hg
  | .letE n v b => by
    intro h g hg
    simp only [isTaintedE, Bool.or_eq_false_iff] at h
    simp only [collectCalls, List.mem_append] at hg
    rcases hg with hg | hg
    · exact collectCalls_unt T v h.1 g hg
    · exact collectCalls_unt T b h.2 g hg
  | .not i => by
    intro h g hg
    simp only [isTaintedE] at h
    simp only [collectCalls] at hg
    exact collectCalls_unt T i h g hg
  | .ite c t e => by
    intro h g hg
    simp only [isTaintedE, Bool.or_eq_false_iff] at h
    simp only [collectCalls, List.mem_append] at hg
    rcases hg with (hg | hg) | hg
    · exact collectCalls_unt T c h.1.1 g hg
    · exact collectCalls_unt T t h.1.2 g hg
    · exact collectCalls_unt T e h.2 g hg
  | .field b f => by
    intro h g hg
    simp only [isTaintedE] at h
    simp only [collectCalls] at hg
    exact collectCalls_unt T b h g hg

theorem collectCallsL_unt (T : Taint) :
    ∀ (es : List Expr), isTaintedL T es = false →
      ∀ g ∈ collectCallsL es, T.fns.contains g = false
  | [] => by
    intro _ g hg
    simp [collectCallsL] at hg
  | a :: as => by
    intro h g hg
    simp only [isTaintedL, Bool.or_eq_false_iff] at h
    simp only [collectCallsL, List.mem_append] at hg
    rcases hg with hg | hg
    · exact collectCalls_unt T a h.1 g hg
    · exact collectCallsL_unt T as h.2 g hg
end

/-- The transitive read set of an untainted expression is untainted — the
key fact that ties `compDeps` (recomputation triggers) to the taint
labeling.  Needs closure: untainted callees have untainted bodies. -/
theorem collectReads_unt {D : Doc} {T : Taint} (hfc : FnClosure D T) :
    ∀ (fuel : Nat) (e : Expr), isTaintedE T e = false →
      ∀ s ∈ collectReads D fuel e, T.vars.contains s = false := by
  intro fuel
  induction fuel with
  | zero =>
    intro e h s hs
    rw [collectReads] at hs
    exact collectVars_unt T e h s hs
  | succ k ih =>
    intro e h s hs
    rw [collectReads] at hs
    rcases List.mem_append.mp hs with hs | hs
    · exact collectVars_unt T e h s hs
    · rcases List.mem_flatMap.mp hs with ⟨g, hg, hsg⟩
      have hgf : T.fns.contains g = false := collectCalls_unt T e h g hg
      rcases hlk : alookup D.functions g with _ | fd
      · rw [hlk] at hsg
        cases hsg
      · rw [hlk] at hsg
        exact ih fd.body (hfc g fd hlk hgf) s hsg

end Mizu
