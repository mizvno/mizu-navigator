import MizuFormal.Semantics
import MizuFormal.Layout

/-!
# T1 — Bounded reaction

The honest termination result.  Structural termination of `Expr` evaluation
would be nearly free (finite trees, DAG call graph) and *misleading*: the
`each`-amplification finding shows resource blowup that stays perfectly
acyclic.  What matters — and what this file proves — is a **data-independent
resource bound**, and it holds *only because of the budgets*
(`MAX_INSTRUCTIONS`, `MAX_SYNTHETIC_LAYOUT_NODES`):

* `eval_out` — the master invariant: along any evaluation, performed work
  never exceeds the instruction budget `B` (every unit of work is pre-charged
  and only performed if the charged counter is still ≤ B);
* `eval_nf` — fuel adequacy: with the fuel `execAction` actually uses
  (`B + 2`), the model's structural fuel can never run out, because every
  recursion step charges at least one instruction and the budget check trips
  first.  Hence every reaction reaches a value, a defined error, or
  `Err.timeout` (`BudgetExceeded`);
* `reaction_work_le` — a whole reaction (action + comp recomputation)
  performs at most `(1 + #comps) · B` units of expression work;
* `T1_reaction_bound` — adding one frame's layout expansion, total resource
  consumption is `≤ (1 + #comps) · B + N`, a bound depending only on the
  *document* (through `#comps`), never on the size of any remote datum.
-/

namespace Mizu

/-- The state invariant carried through evaluation: performed work is covered
by the charged counter, and performed work is within budget. -/
abbrev Inv (B : Nat) (s : EvalSt) : Prop := s.work ≤ s.count ∧ s.work ≤ B

/-- No effect in the list is a navigation.  Expression evaluation can emit
`storeLocal` / `copyClipboard` / `download` but never `navigate` — the
asymmetry that makes the flow checker's sink set complete (T2). -/
def navFree (l : List Effect) : Prop := ∀ eff ∈ l, eff.isNav = false

theorem navFree_nil : navFree [] := by intro e h; cases h

theorem navFree_append {a b : List Effect} (ha : navFree a) (hb : navFree b) :
    navFree (a ++ b) := by
  intro e he
  rcases List.mem_append.mp he with h | h
  · exact ha e h
  · exact hb e h

/-- Everything `eval_out` establishes about one evaluator call
`s ⟶ r = (outcome, s')`. -/
structure Out (B : Nat) (s : EvalSt) (r : Res α) : Prop where
  /-- The instruction counter never decreases. -/
  mono : s.count ≤ r.2.count
  /-- Work stays covered by the counter. -/
  wc : r.2.work ≤ r.2.count
  /-- **The budget theorem**: performed work never exceeds `B`. -/
  wb : r.2.work ≤ B
  /-- A successful outcome leaves the counter within budget (given it started
  within budget) — successful continuations always resume under the cliff. -/
  okb : ∀ v, r.1 = Except.ok v → s.count ≤ B → r.2.count ≤ B
  /-- Effects are append-only, and everything appended is navigation-free. -/
  eff : ∃ Δ, r.2.effects = s.effects ++ Δ ∧ navFree Δ

namespace Out

/-- Identity step returning `ok` (no state change). -/
theorem idOk {B : Nat} {s : EvalSt} (h : Inv B s) (v : α) :
    Out B s ((Except.ok v, s) : Res α) :=
  ⟨Nat.le_refl _, h.1, h.2, (fun _ _ hs => hs), ⟨[], by simp, navFree_nil⟩⟩

/-- Identity step returning an error (no state change). -/
theorem idErr {B : Nat} {s : EvalSt} (h : Inv B s) (er : Err) :
    Out B s ((Except.error er, s) : Res α) :=
  ⟨Nat.le_refl _, h.1, h.2, (fun _ hv => nomatch hv), ⟨[], by simp, navFree_nil⟩⟩

theorem inv {B : Nat} {s : EvalSt} {r : Res α} (h : Out B s r) : Inv B r.2 :=
  ⟨h.wc, h.wb⟩

/-- Sequencing through a successful intermediate state. -/
theorem step {B : Nat} {s s1 : EvalSt} {v : α} {r2 : Res β}
    (h1 : Out B s ((Except.ok v, s1) : Res α)) (h2 : Out B s1 r2) : Out B s r2 :=
  { mono := Nat.le_trans h1.mono h2.mono
    wc := h2.wc
    wb := h2.wb
    okb := fun w hw hs => h2.okb w hw (h1.okb v rfl hs)
    eff := by
      obtain ⟨d1, e1, n1⟩ := h1.eff
      obtain ⟨d2, e2, n2⟩ := h2.eff
      have e1' : s1.effects = s.effects ++ d1 := e1
      exact ⟨d1 ++ d2, by rw [e2, e1', List.append_assoc], navFree_append n1 n2⟩ }

/-- Replace the payload of a successful result (pure post-processing). -/
theorem retOk {B : Nat} {s s1 : EvalSt} {u : α} (h : Out B s ((Except.ok u, s1) : Res α))
    (v : β) : Out B s ((Except.ok v, s1) : Res β) :=
  ⟨h.mono, h.wc, h.wb, (fun _ _ hs => h.okb u rfl hs), h.eff⟩

/-- Turn a successful result state into an error result. -/
theorem retErr {B : Nat} {s s1 : EvalSt} {u : α} (h : Out B s ((Except.ok u, s1) : Res α))
    (er : Err) : Out B s ((Except.error er, s1) : Res β) :=
  ⟨h.mono, h.wc, h.wb, (fun _ hv => nomatch hv), h.eff⟩

/-- Transport an error result across the payload type. -/
theorem errCast {B : Nat} {s s1 : EvalSt} {er : Err}
    (h : Out B s ((Except.error er, s1) : Res α)) :
    Out B s ((Except.error er, s1) : Res β) :=
  ⟨h.mono, h.wc, h.wb, (fun _ hv => nomatch hv), h.eff⟩

/-- **`evalDepth` is invisible to `Out`** (RM-09): none of `Out`'s five
components — `mono`/`okb` on `s.count`, `wc`/`wb`/`okb` on `r.2.count`/
`r.2.work`, `eff` on `r.2.effects` — ever inspects `evalDepth`, on either the
base state or the result state. So overwriting the result state's
`evalDepth` field (to bump it on entry to `evalE`'s recursive dispatch, or to
undo that bump on the way back out, mirroring `eval_depth += 1` / `-= 1`
around `evaluate_impl` in `types.rs::StateMachine::evaluate`) preserves
`Out` for free. -/
theorem setDepth {B : Nat} {s r2 : EvalSt} {out : Except Err α} (d : Nat)
    (h : Out B s ((out, r2) : Res α)) :
    Out B s ((out, { r2 with evalDepth := d }) : Res α) :=
  ⟨h.mono, h.wc, h.wb, h.okb, h.eff⟩

/-- Append one navigation-free effect to a successful result. -/
theorem emitOk {B : Nat} {s s1 : EvalSt} {u : α} (h : Out B s ((Except.ok u, s1) : Res α))
    {eff : Effect} (heff : eff.isNav = false) (v : β) :
    Out B s ((Except.ok v, emit eff s1) : Res β) :=
  { mono := h.mono
    wc := h.wc
    wb := h.wb
    okb := fun _ _ hs => h.okb u rfl hs
    eff := by
      obtain ⟨d, e, n⟩ := h.eff
      have e' : s1.effects = s.effects ++ d := e
      refine ⟨d ++ [eff], ?_, navFree_append n ?_⟩
      · show s1.effects ++ [eff] = s.effects ++ (d ++ [eff])
        rw [e', List.append_assoc]
      · intro x hx
        simp at hx
        subst hx
        exact heff }

end Out

/-- Charging respects the invariant: work advances only under a passed check. -/
theorem charge_out {B : Nat} (n : Nat) {s : EvalSt} (h : Inv B s) :
    Out B s (charge B n s) := by
  have h1 := h.1
  have h2 := h.2
  unfold charge
  split
  · rename_i hgt
    exact
      { mono := Nat.le_add_right _ _
        wc := by show s.work ≤ s.count + n; omega
        wb := h2
        okb := fun _ hv => nomatch hv
        eff := ⟨[], by simp, navFree_nil⟩ }
  · rename_i hle
    exact
      { mono := Nat.le_add_right _ _
        wc := by show s.work + n ≤ s.count + n; omega
        wb := by show s.work + n ≤ B; omega
        okb := fun _ _ _ => by show s.count + n ≤ B; omega
        eff := ⟨[], by simp, navFree_nil⟩ }

/-- On success, `charge` advances the counter by exactly `n` and stays within
budget. -/
theorem charge_ok {B n : Nat} {s s' : EvalSt} {u : Unit}
    (h : charge B n s = (Except.ok u, s')) :
    s'.count = s.count + n ∧ s'.count ≤ B := by
  unfold charge at h
  split at h
  · exact nomatch h
  · rename_i hle
    injection h with h1 h2
    subst h2
    exact ⟨rfl, by show s.count + n ≤ B; omega⟩

/-! ## The master invariant, modularly

Each lemma below is parametric in the `evalE` fact `He` at the *same* fuel;
`eval_out` closes the loop by induction on fuel. -/

section OutLemmas

variable {B : Nat} {D : Doc} {σ : Store} {fuel : Nat}

theorem evalArgs_out
    (He : ∀ env e s, Inv B s → Out B s (evalE B D σ fuel env e s)) :
    ∀ (args : List Expr) (env : Env) (s : EvalSt), Inv B s →
      Out B s (evalArgs B D σ fuel env args s) := by
  intro args
  induction args with
  | nil =>
    intro env s h
    rw [evalArgs]
    exact Out.idOk h ([] : List Val)
  | cons a rest ih =>
    intro env s h
    rw [evalArgs]
    split
    · rename_i er s1 heq
      exact (show Out B s ((Except.error er, s1) : Res Val) from heq ▸ He env a s h).errCast
    · rename_i v s1 heq
      have o1 : Out B s ((Except.ok v, s1) : Res Val) := heq ▸ He env a s h
      split
      · rename_i er s2 heq2
        exact o1.step (show Out B s1 ((Except.error er, s2) : Res (List Val)) from
          heq2 ▸ ih env s1 o1.inv)
      · rename_i vs s2 heq2
        exact (o1.step (show Out B s1 ((Except.ok vs, s2) : Res (List Val)) from
          heq2 ▸ ih env s1 o1.inv)).retOk (v :: vs)

theorem evalStoreLocal_out
    (He : ∀ env e s, Inv B s → Out B s (evalE B D σ fuel env e s))
    (env : Env) (k v : Expr) (s : EvalSt) (h : Inv B s) :
    Out B s (evalStoreLocal B D σ fuel env k v s) := by
  rw [evalStoreLocal]
  split
  · rename_i er s1 heq
    exact heq ▸ He env k s h
  · rename_i kv s1 heq
    have o1 : Out B s ((Except.ok kv, s1) : Res Val) := heq ▸ He env k s h
    split
    next ks =>
      split
      · rename_i er s2 heq2
        exact o1.step (heq2 ▸ He env v s1 o1.inv)
      · rename_i vv s2 heq2
        exact o1.step
          ((show Out B s1 ((Except.ok vv, s2) : Res Val) from heq2 ▸ He env v s1 o1.inv).emitOk
            rfl _)
    all_goals exact o1.retErr _

theorem evalClip_out
    (He : ∀ env e s, Inv B s → Out B s (evalE B D σ fuel env e s))
    (env : Env) (x : Expr) (s : EvalSt) (h : Inv B s) :
    Out B s (evalClip B D σ fuel env x s) := by
  rw [evalClip]
  split
  · rename_i er s1 heq
    exact heq ▸ He env x s h
  · rename_i v s1 heq
    have o1 : Out B s ((Except.ok v, s1) : Res Val) := heq ▸ He env x s h
    split
    next nodeId => exact o1.emitOk rfl _
    all_goals exact o1.retErr _

theorem evalDownload_out {B : Nat} (a : Expr) {s : EvalSt} (h : Inv B s) :
    Out B s (evalDownload a s) := by
  rw [evalDownload.eq_def]
  split
  · rename_i aliasSym
    exact (Out.idOk h Val.null).emitOk rfl _
  · exact Out.idErr h _

theorem evalGetSystemTime_out {B : Nat} {D : Doc} (a : Expr) {s : EvalSt} (h : Inv B s) :
    Out B s (evalGetSystemTime D a s) := by
  rw [evalGetSystemTime.eq_def]
  split
  · rename_i targetSym
    split
    · exact Out.idErr h _
    · exact (Out.idOk h (Val.bool true)).emitOk rfl _
  · exact Out.idErr h _

theorem evalFilter_out
    (He : ∀ env e s, Inv B s → Out B s (evalE B D σ fuel env e s))
    (env : Env) (l f t : Expr) (s : EvalSt) (h : Inv B s) :
    Out B s (evalFilter B D σ fuel env l f t s) := by
  rw [evalFilter]
  split
  · rename_i er s1 heq
    exact heq ▸ He env l s h
  · rename_i lv s1 heq
    have o1 : Out B s ((Except.ok lv, s1) : Res Val) := heq ▸ He env l s h
    split
    · rename_i er s2 heq2
      exact o1.step (heq2 ▸ He env f s1 o1.inv)
    · rename_i fv s2 heq2
      have o2 : Out B s1 ((Except.ok fv, s2) : Res Val) := heq2 ▸ He env f s1 o1.inv
      split
      · rename_i er s3 heq3
        exact o1.step (o2.step (heq3 ▸ He env t s2 o2.inv))
      · rename_i tv s3 heq3
        have o3 : Out B s2 ((Except.ok tv, s3) : Res Val) := heq3 ▸ He env t s2 o2.inv
        have o13 : Out B s ((Except.ok tv, s3) : Res Val) := o1.step (o2.step o3)
        split
        next xs =>
          split
          · rename_i er s4 heq4
            exact o13.step
              ((show Out B s3 ((Except.error er, s4) : Res Unit) from
                heq4 ▸ charge_out xs.length o13.inv).errCast)
          · rename_i u s4 heq4
            have o4 : Out B s3 ((Except.ok u, s4) : Res Unit) :=
              heq4 ▸ charge_out xs.length o13.inv
            split
            next fs => exact o13.step (o4.retOk _)
            all_goals exact o13.step (o4.retErr _)
        all_goals exact o13.retErr _

theorem evalCount_out
    (He : ∀ env e s, Inv B s → Out B s (evalE B D σ fuel env e s))
    (env : Env) (l f t : Expr) (s : EvalSt) (h : Inv B s) :
    Out B s (evalCount B D σ fuel env l f t s) := by
  rw [evalCount]
  split
  · rename_i er s1 heq
    exact heq ▸ He env l s h
  · rename_i lv s1 heq
    have o1 : Out B s ((Except.ok lv, s1) : Res Val) := heq ▸ He env l s h
    split
    · rename_i er s2 heq2
      exact o1.step (heq2 ▸ He env f s1 o1.inv)
    · rename_i fv s2 heq2
      have o2 : Out B s1 ((Except.ok fv, s2) : Res Val) := heq2 ▸ He env f s1 o1.inv
      split
      · rename_i er s3 heq3
        exact o1.step (o2.step (heq3 ▸ He env t s2 o2.inv))
      · rename_i tv s3 heq3
        have o3 : Out B s2 ((Except.ok tv, s3) : Res Val) := heq3 ▸ He env t s2 o2.inv
        have o13 : Out B s ((Except.ok tv, s3) : Res Val) := o1.step (o2.step o3)
        split
        next xs =>
          split
          · rename_i er s4 heq4
            exact o13.step
              ((show Out B s3 ((Except.error er, s4) : Res Unit) from
                heq4 ▸ charge_out xs.length o13.inv).errCast)
          · rename_i u s4 heq4
            have o4 : Out B s3 ((Except.ok u, s4) : Res Unit) :=
              heq4 ▸ charge_out xs.length o13.inv
            split
            next fs => exact o13.step (o4.retOk _)
            all_goals exact o13.step (o4.retErr _)
        all_goals exact o13.retErr _

theorem evalSort_out
    (He : ∀ env e s, Inv B s → Out B s (evalE B D σ fuel env e s))
    (env : Env) (l f dir : Expr) (s : EvalSt) (h : Inv B s) :
    Out B s (evalSort B D σ fuel env l f dir s) := by
  rw [evalSort]
  split
  · rename_i er s1 heq
    exact heq ▸ He env l s h
  · rename_i lv s1 heq
    have o1 : Out B s ((Except.ok lv, s1) : Res Val) := heq ▸ He env l s h
    split
    · rename_i er s2 heq2
      exact o1.step (heq2 ▸ He env f s1 o1.inv)
    · rename_i fv s2 heq2
      have o2 : Out B s1 ((Except.ok fv, s2) : Res Val) := heq2 ▸ He env f s1 o1.inv
      split
      · rename_i er s3 heq3
        exact o1.step (o2.step (heq3 ▸ He env dir s2 o2.inv))
      · rename_i dv s3 heq3
        have o3 : Out B s2 ((Except.ok dv, s3) : Res Val) := heq3 ▸ He env dir s2 o2.inv
        have o13 : Out B s ((Except.ok dv, s3) : Res Val) := o1.step (o2.step o3)
        split
        next xs =>
          split
          · rename_i er s4 heq4
            exact o13.step
              ((show Out B s3 ((Except.error er, s4) : Res Unit) from
                heq4 ▸ charge_out (sortCost xs.length) o13.inv).errCast)
          · rename_i u s4 heq4
            have o4 : Out B s3 ((Except.ok u, s4) : Res Unit) :=
              heq4 ▸ charge_out (sortCost xs.length) o13.inv
            split
            next fs =>
              split
              next d =>
                split
                · exact o13.step (o4.retOk _)
                · exact o13.step (o4.retErr _)
              all_goals exact o13.step (o4.retErr _)
            all_goals exact o13.step (o4.retErr _)
        all_goals exact o13.retErr _

theorem evalUser_out
    (He : ∀ env e s, Inv B s → Out B s (evalE B D σ fuel env e s))
    (env : Env) (fname : Symbol) (args : List Expr) (s : EvalSt) (h : Inv B s) :
    Out B s (evalUser B D σ fuel env fname args s) := by
  rw [evalUser]
  split
  · exact Out.idErr h _
  · rename_i fd heq
    split
    · split
      · rename_i er s1 heq2
        exact (show Out B s ((Except.error er, s1) : Res (List Val)) from
          heq2 ▸ evalArgs_out He args env s h).errCast
      · rename_i vals s1 heq2
        have o1 : Out B s ((Except.ok vals, s1) : Res (List Val)) :=
          heq2 ▸ evalArgs_out He args env s h
        exact o1.step (He _ _ s1 o1.inv)
    · exact Out.idErr h _

theorem evalCall_out
    (He : ∀ env e s, Inv B s → Out B s (evalE B D σ fuel env e s))
    (env : Env) (fname : Symbol) (args : List Expr) (s : EvalSt) (h : Inv B s) :
    Out B s (evalCall B D σ fuel env fname args s) := by
  rw [evalCall.eq_def]
  split
  · -- storeLocal
    split
    · rename_i k v
      exact evalStoreLocal_out He env k v s h
    · exact Out.idErr h _
  · -- copyClipboard
    split
    · rename_i x
      exact evalClip_out He env x s h
    · exact Out.idErr h _
  · -- download
    split
    · rename_i a
      exact evalDownload_out a h
    · exact evalUser_out He env fname _ s h
  · -- getSystemTime
    split
    · rename_i a
      exact evalGetSystemTime_out a h
    · exact Out.idErr h _
  · -- filter
    split
    · rename_i l f t
      exact evalFilter_out He env l f t s h
    · exact evalUser_out He env fname _ s h
  · -- count
    split
    · rename_i l f t
      exact evalCount_out He env l f t s h
    · exact evalUser_out He env fname _ s h
  · -- sort
    split
    · rename_i l f d
      exact evalSort_out He env l f d s h
    · exact evalUser_out He env fname _ s h
  · -- none
    exact evalUser_out He env fname args s h

end OutLemmas

/-- **Master invariant** for the evaluator, by induction on fuel. -/
theorem eval_out (B : Nat) (D : Doc) (σ : Store) :
    ∀ (fuel : Nat) (env : Env) (e : Expr) (s : EvalSt), Inv B s →
      Out B s (evalE B D σ fuel env e s) := by
  intro fuel
  induction fuel with
  | zero =>
    intro env e s h
    rw [evalE]
    exact Out.idErr h _
  | succ f ihf =>
    intro env e s hInv
    rw [evalE]
    split
    · rename_i er s1 hch
      exact (show Out B s ((Except.error er, s1) : Res Unit) from
        hch ▸ charge_out 1 hInv).errCast
    · rename_i u s1 hch
      have och : Out B s ((Except.ok u, s1) : Res Unit) := hch ▸ charge_out 1 hInv
      have hInv1 : Inv B s1 := och.inv
      split
      · -- evalDepth guard tripped (RM-09): state unchanged (still s1), error result.
        exact och.retErr _
      · -- evalDepth guard passed: bump depth for the recursive dispatch on `e`,
        -- then undo the bump on every return path via `Out.setDepth` — mirrors
        -- `eval_depth += 1` before / `-= 1` after `evaluate_impl`
        -- (`types.rs::StateMachine::evaluate`).
        rename_i hdepth
        let s1' := { s1 with evalDepth := s1.evalDepth + 1 }
        have hInv1' : Inv B s1' := hInv1
        have och' : Out B s ((Except.ok u, s1') : Res Unit) := Out.setDepth (s1.evalDepth + 1) och
        refine Out.setDepth _ ?_
        split
        · -- lit
          rename_i v
          exact och'.retOk v
        · -- var
          rename_i sym
          split
          · rename_i v heq
            exact och'.retOk _
          · split
            · exact och'.retErr _
            · rename_i v heq2
              split
              next => exact och'.retErr _
              all_goals exact och'.retOk _
        · -- binop
          rename_i op l r
          split
          · rename_i er s2 heq
            exact och'.step (heq ▸ ihf env l s1' hInv1')
          · rename_i lv s2 heq
            have o1 : Out B s1' ((Except.ok lv, s2) : Res Val) := heq ▸ ihf env l s1' hInv1'
            split
            · rename_i er s3 heq2
              exact och'.step (o1.step (heq2 ▸ ihf env r s2 o1.inv))
            · rename_i rv s3 heq2
              have o2 : Out B s2 ((Except.ok rv, s3) : Res Val) := heq2 ▸ ihf env r s2 o1.inv
              have o12 : Out B s1' ((Except.ok rv, s3) : Res Val) := o1.step o2
              split
              · rename_i er2 s4 heq3
                exact och'.step (o12.step
                  ((show Out B s3 ((Except.error er2, s4) : Res Unit) from
                    heq3 ▸ charge_out (binopCost op lv rv) o12.inv).errCast))
              · rename_i u s4 heq3
                have o3 : Out B s3 ((Except.ok u, s4) : Res Unit) :=
                  heq3 ▸ charge_out (binopCost op lv rv) o12.inv
                have o123 := o12.step o3
                split
                · rename_i v heq4
                  exact och'.step (o123.retOk v)
                · rename_i er heq4
                  exact och'.step (o123.retErr er)
        · -- not
          rename_i inner
          split
          · rename_i er s2 heq
            exact och'.step (heq ▸ ihf env inner s1' hInv1')
          · rename_i v s2 heq
            have o1 : Out B s1' ((Except.ok v, s2) : Res Val) := heq ▸ ihf env inner s1' hInv1'
            split
            next b => exact och'.step (o1.retOk _)
            all_goals exact och'.step (o1.retErr _)
        · -- ite
          rename_i c t el
          split
          · rename_i er s2 heq
            exact och'.step (heq ▸ ihf env c s1' hInv1')
          · rename_i v s2 heq
            have o1 : Out B s1' ((Except.ok v, s2) : Res Val) := heq ▸ ihf env c s1' hInv1'
            split
            next => exact och'.step (o1.step (ihf env t s2 o1.inv))
            next => exact och'.step (o1.step (ihf env el s2 o1.inv))
            all_goals exact och'.step (o1.retErr _)
        · -- field
          rename_i base fname
          split
          · rename_i er s2 heq
            exact och'.step (heq ▸ ihf env base s1' hInv1')
          · rename_i v s2 heq
            have o1 : Out B s1' ((Except.ok v, s2) : Res Val) := heq ▸ ihf env base s1' hInv1'
            split
            next fs =>
              split
              · rename_i fv heq2
                exact och'.step (o1.retOk _)
              · exact och'.step (o1.retErr _)
            all_goals exact och'.step (o1.retErr _)
        · -- letE
          rename_i name v body
          split
          · rename_i er s2 heq
            exact och'.step (heq ▸ ihf env v s1' hInv1')
          · rename_i bv s2 heq
            have o1 : Out B s1' ((Except.ok bv, s2) : Res Val) := heq ▸ ihf env v s1' hInv1'
            exact och'.step (o1.step (ihf ((name, bv) :: env) body s2 o1.inv))
        · -- call
          rename_i fname args
          exact och'.step (evalCall_out ihf env fname args s1' hInv1')

/-- Work performed by one evaluator call is within budget. -/
theorem eval_work_le (B : Nat) (D : Doc) (σ : Store) (fuel : Nat) (env : Env)
    (e : Expr) (s : EvalSt) (h : Inv B s) :
    (evalE B D σ fuel env e s).2.work ≤ B :=
  (eval_out B D σ fuel env e s h).wb

/-! ## Fuel adequacy

`Err.fuel` is a *model artifact*: the structural fuel that makes the
evaluator total in Lean.  The lemmas below prove it unreachable at the fuel
`execAction` actually supplies (`B + 2`), because every recursion step
charges at least one instruction, so the `Err.timeout` cliff always trips
first.  Consequently the fuel is no part of the modeled language: every
reaction reaches a value, a defined error, or `BudgetExceeded`. -/

abbrev NF (r : Res α) : Prop := r.1 ≠ Except.error Err.fuel

theorem nf_ok {α : Type} {v : α} {s : EvalSt} : NF ((Except.ok v, s) : Res α) := by
  intro h
  nomatch h

theorem nf_pair {α : Type} {er : Err} {s : EvalSt} (h : er ≠ Err.fuel) :
    NF ((Except.error er, s) : Res α) := by
  intro hc
  exact h (by injection hc)

theorem nf_err {α : Type} {r : Res α} (h : NF r) {er : Err} {s' : EvalSt}
    (heq : r = (Except.error er, s')) : er ≠ Err.fuel := by
  intro he
  apply h
  rw [heq, he]

/-- `NF` never inspects the result state at all (only `r.1`, RM-09), so
overwriting its `evalDepth` field — the entry bump / exit undo around
`evalE`'s recursive dispatch — is transparent to it, exactly like
`Out.setDepth` is for `Out`. -/
theorem nf_setDepth {α : Type} {r2 : EvalSt} {out : Except Err α} (d : Nat)
    (h : NF ((out, r2) : Res α)) : NF ((out, { r2 with evalDepth := d }) : Res α) := h

theorem charge_nf {B n : Nat} {s : EvalSt} : NF (charge B n s) := by
  unfold charge
  split
  · exact nf_pair (fun h => nomatch h)
  · exact nf_ok

theorem applyBinop_nf (op : BinOp) (a b : Val) :
    applyBinop op a b ≠ Except.error Err.fuel := by
  rw [applyBinop.eq_def]
  split <;> (try split) <;> (try split) <;> simp

section NfLemmas

variable {B : Nat} {D : Doc} {σ : Store} {fuel : Nat}

theorem evalArgs_nf
    (He : ∀ env e s, Inv B s → Out B s (evalE B D σ fuel env e s))
    (Hnf : ∀ env e s, Inv B s → s.count ≤ B → B + 2 ≤ fuel + s.count →
      NF (evalE B D σ fuel env e s)) :
    ∀ (args : List Expr) (env : Env) (s : EvalSt), Inv B s → s.count ≤ B →
      B + 2 ≤ fuel + s.count → NF (evalArgs B D σ fuel env args s) := by
  intro args
  induction args with
  | nil =>
    intro env s _ _ _
    rw [evalArgs]
    exact nf_ok
  | cons a rest ih =>
    intro env s h hc hf
    rw [evalArgs]
    split
    · rename_i er s1 heq
      exact nf_pair (nf_err (Hnf env a s h hc hf) heq)
    · rename_i v s1 heq
      have o1 : Out B s ((Except.ok v, s1) : Res Val) := heq ▸ He env a s h
      have hc1 : s1.count ≤ B := o1.okb v rfl hc
      have hm : s.count ≤ s1.count := o1.mono
      split
      · rename_i er s2 heq2
        exact nf_pair (nf_err (ih env s1 o1.inv hc1 (by omega)) heq2)
      · exact nf_ok

theorem evalStoreLocal_nf
    (He : ∀ env e s, Inv B s → Out B s (evalE B D σ fuel env e s))
    (Hnf : ∀ env e s, Inv B s → s.count ≤ B → B + 2 ≤ fuel + s.count →
      NF (evalE B D σ fuel env e s))
    (env : Env) (k v : Expr) (s : EvalSt) (h : Inv B s) (hc : s.count ≤ B)
    (hf : B + 2 ≤ fuel + s.count) :
    NF (evalStoreLocal B D σ fuel env k v s) := by
  rw [evalStoreLocal]
  split
  · rename_i er s1 heq
    exact nf_pair (nf_err (Hnf env k s h hc hf) heq)
  · rename_i kv s1 heq
    have o1 : Out B s ((Except.ok kv, s1) : Res Val) := heq ▸ He env k s h
    have hc1 : s1.count ≤ B := o1.okb kv rfl hc
    have hm : s.count ≤ s1.count := o1.mono
    split
    · rename_i ks
      split
      · rename_i er s2 heq2
        exact nf_pair (nf_err (Hnf env v s1 o1.inv hc1 (by omega)) heq2)
      · exact nf_ok
    · exact nf_pair (fun hh => nomatch hh)

theorem evalClip_nf
    (He : ∀ env e s, Inv B s → Out B s (evalE B D σ fuel env e s))
    (Hnf : ∀ env e s, Inv B s → s.count ≤ B → B + 2 ≤ fuel + s.count →
      NF (evalE B D σ fuel env e s))
    (env : Env) (x : Expr) (s : EvalSt) (h : Inv B s) (hc : s.count ≤ B)
    (hf : B + 2 ≤ fuel + s.count) :
    NF (evalClip B D σ fuel env x s) := by
  rw [evalClip]
  split
  · rename_i er s1 heq
    exact nf_pair (nf_err (Hnf env x s h hc hf) heq)
  · rename_i v s1 heq
    split
    · exact nf_ok
    · exact nf_pair (fun hh => nomatch hh)

theorem evalDownload_nf (a : Expr) (s : EvalSt) : NF (evalDownload a s) := by
  rw [evalDownload.eq_def]
  split
  · exact nf_ok
  · exact nf_pair (fun hh => nomatch hh)

theorem evalGetSystemTime_nf (D : Doc) (a : Expr) (s : EvalSt) :
    NF (evalGetSystemTime D a s) := by
  rw [evalGetSystemTime.eq_def]
  split
  · split
    · exact nf_pair (fun hh => nomatch hh)
    · exact nf_ok
  · exact nf_pair (fun hh => nomatch hh)

theorem evalFilter_nf
    (He : ∀ env e s, Inv B s → Out B s (evalE B D σ fuel env e s))
    (Hnf : ∀ env e s, Inv B s → s.count ≤ B → B + 2 ≤ fuel + s.count →
      NF (evalE B D σ fuel env e s))
    (env : Env) (l f t : Expr) (s : EvalSt) (h : Inv B s) (hc : s.count ≤ B)
    (hf : B + 2 ≤ fuel + s.count) :
    NF (evalFilter B D σ fuel env l f t s) := by
  rw [evalFilter]
  split
  · rename_i er s1 heq
    exact nf_pair (nf_err (Hnf env l s h hc hf) heq)
  · rename_i lv s1 heq
    have o1 : Out B s ((Except.ok lv, s1) : Res Val) := heq ▸ He env l s h
    have hc1 : s1.count ≤ B := o1.okb lv rfl hc
    have hm1 : s.count ≤ s1.count := o1.mono
    split
    · rename_i er s2 heq2
      exact nf_pair (nf_err (Hnf env f s1 o1.inv hc1 (by omega)) heq2)
    · rename_i fv s2 heq2
      have o2 : Out B s1 ((Except.ok fv, s2) : Res Val) := heq2 ▸ He env f s1 o1.inv
      have hc2 : s2.count ≤ B := o2.okb fv rfl hc1
      have hm2 : s1.count ≤ s2.count := o2.mono
      split
      · rename_i er s3 heq3
        exact nf_pair (nf_err (Hnf env t s2 o2.inv hc2 (by omega)) heq3)
      · rename_i tv s3 heq3
        split
        · rename_i xs
          split
          · rename_i er s4 heq4
            exact nf_pair (nf_err charge_nf heq4)
          · rename_i u s4 heq4
            split
            · exact nf_ok
            · exact nf_pair (fun hh => nomatch hh)
        · exact nf_pair (fun hh => nomatch hh)

theorem evalCount_nf
    (He : ∀ env e s, Inv B s → Out B s (evalE B D σ fuel env e s))
    (Hnf : ∀ env e s, Inv B s → s.count ≤ B → B + 2 ≤ fuel + s.count →
      NF (evalE B D σ fuel env e s))
    (env : Env) (l f t : Expr) (s : EvalSt) (h : Inv B s) (hc : s.count ≤ B)
    (hf : B + 2 ≤ fuel + s.count) :
    NF (evalCount B D σ fuel env l f t s) := by
  rw [evalCount]
  split
  · rename_i er s1 heq
    exact nf_pair (nf_err (Hnf env l s h hc hf) heq)
  · rename_i lv s1 heq
    have o1 : Out B s ((Except.ok lv, s1) : Res Val) := heq ▸ He env l s h
    have hc1 : s1.count ≤ B := o1.okb lv rfl hc
    have hm1 : s.count ≤ s1.count := o1.mono
    split
    · rename_i er s2 heq2
      exact nf_pair (nf_err (Hnf env f s1 o1.inv hc1 (by omega)) heq2)
    · rename_i fv s2 heq2
      have o2 : Out B s1 ((Except.ok fv, s2) : Res Val) := heq2 ▸ He env f s1 o1.inv
      have hc2 : s2.count ≤ B := o2.okb fv rfl hc1
      have hm2 : s1.count ≤ s2.count := o2.mono
      split
      · rename_i er s3 heq3
        exact nf_pair (nf_err (Hnf env t s2 o2.inv hc2 (by omega)) heq3)
      · rename_i tv s3 heq3
        split
        · rename_i xs
          split
          · rename_i er s4 heq4
            exact nf_pair (nf_err charge_nf heq4)
          · rename_i u s4 heq4
            split
            · exact nf_ok
            · exact nf_pair (fun hh => nomatch hh)
        · exact nf_pair (fun hh => nomatch hh)

theorem evalSort_nf
    (He : ∀ env e s, Inv B s → Out B s (evalE B D σ fuel env e s))
    (Hnf : ∀ env e s, Inv B s → s.count ≤ B → B + 2 ≤ fuel + s.count →
      NF (evalE B D σ fuel env e s))
    (env : Env) (l f dir : Expr) (s : EvalSt) (h : Inv B s) (hc : s.count ≤ B)
    (hf : B + 2 ≤ fuel + s.count) :
    NF (evalSort B D σ fuel env l f dir s) := by
  rw [evalSort]
  split
  · rename_i er s1 heq
    exact nf_pair (nf_err (Hnf env l s h hc hf) heq)
  · rename_i lv s1 heq
    have o1 : Out B s ((Except.ok lv, s1) : Res Val) := heq ▸ He env l s h
    have hc1 : s1.count ≤ B := o1.okb lv rfl hc
    have hm1 : s.count ≤ s1.count := o1.mono
    split
    · rename_i er s2 heq2
      exact nf_pair (nf_err (Hnf env f s1 o1.inv hc1 (by omega)) heq2)
    · rename_i fv s2 heq2
      have o2 : Out B s1 ((Except.ok fv, s2) : Res Val) := heq2 ▸ He env f s1 o1.inv
      have hc2 : s2.count ≤ B := o2.okb fv rfl hc1
      have hm2 : s1.count ≤ s2.count := o2.mono
      split
      · rename_i er s3 heq3
        exact nf_pair (nf_err (Hnf env dir s2 o2.inv hc2 (by omega)) heq3)
      · rename_i dv s3 heq3
        split
        · rename_i xs
          split
          · rename_i er s4 heq4
            exact nf_pair (nf_err charge_nf heq4)
          · rename_i u s4 heq4
            split
            · rename_i fs
              split
              · rename_i d
                split
                · exact nf_ok
                · exact nf_pair (fun hh => nomatch hh)
              · exact nf_pair (fun hh => nomatch hh)
            · exact nf_pair (fun hh => nomatch hh)
        · exact nf_pair (fun hh => nomatch hh)

theorem evalUser_nf
    (He : ∀ env e s, Inv B s → Out B s (evalE B D σ fuel env e s))
    (Hnf : ∀ env e s, Inv B s → s.count ≤ B → B + 2 ≤ fuel + s.count →
      NF (evalE B D σ fuel env e s))
    (env : Env) (fname : Symbol) (args : List Expr) (s : EvalSt) (h : Inv B s)
    (hc : s.count ≤ B) (hf : B + 2 ≤ fuel + s.count) :
    NF (evalUser B D σ fuel env fname args s) := by
  rw [evalUser]
  split
  · exact nf_pair (fun hh => nomatch hh)
  · rename_i fd heq
    split
    · split
      · rename_i er s1 heq2
        exact nf_pair (nf_err (evalArgs_nf He Hnf args env s h hc hf) heq2)
      · rename_i vals s1 heq2
        have o1 : Out B s ((Except.ok vals, s1) : Res (List Val)) :=
          heq2 ▸ evalArgs_out He args env s h
        have hc1 : s1.count ≤ B := o1.okb vals rfl hc
        have hm : s.count ≤ s1.count := o1.mono
        exact Hnf _ _ s1 o1.inv hc1 (by omega)
    · exact nf_pair (fun hh => nomatch hh)

theorem evalCall_nf
    (He : ∀ env e s, Inv B s → Out B s (evalE B D σ fuel env e s))
    (Hnf : ∀ env e s, Inv B s → s.count ≤ B → B + 2 ≤ fuel + s.count →
      NF (evalE B D σ fuel env e s))
    (env : Env) (fname : Symbol) (args : List Expr) (s : EvalSt) (h : Inv B s)
    (hc : s.count ≤ B) (hf : B + 2 ≤ fuel + s.count) :
    NF (evalCall B D σ fuel env fname args s) := by
  rw [evalCall.eq_def]
  split
  · split
    · rename_i k v
      exact evalStoreLocal_nf He Hnf env k v s h hc hf
    · exact nf_pair (fun hh => nomatch hh)
  · split
    · rename_i x
      exact evalClip_nf He Hnf env x s h hc hf
    · exact nf_pair (fun hh => nomatch hh)
  · split
    · rename_i a
      exact evalDownload_nf a s
    · exact evalUser_nf He Hnf env fname _ s h hc hf
  · split
    · rename_i a
      exact evalGetSystemTime_nf D a s
    · exact nf_pair (fun hh => nomatch hh)
  · split
    · rename_i l f t
      exact evalFilter_nf He Hnf env l f t s h hc hf
    · exact evalUser_nf He Hnf env fname _ s h hc hf
  · split
    · rename_i l f t
      exact evalCount_nf He Hnf env l f t s h hc hf
    · exact evalUser_nf He Hnf env fname _ s h hc hf
  · split
    · rename_i l f d
      exact evalSort_nf He Hnf env l f d s h hc hf
    · exact evalUser_nf He Hnf env fname _ s h hc hf
  · exact evalUser_nf He Hnf env fname args s h hc hf

end NfLemmas

/-- **Fuel adequacy**: with remaining budget headroom covered by fuel, the
fuel artifact is unreachable.  In particular at `fuel = B + 2`, `count = 0`. -/
theorem eval_nf (B : Nat) (D : Doc) (σ : Store) :
    ∀ (fuel : Nat) (env : Env) (e : Expr) (s : EvalSt), Inv B s → s.count ≤ B →
      B + 2 ≤ fuel + s.count → NF (evalE B D σ fuel env e s) := by
  intro fuel
  induction fuel with
  | zero =>
    intro env e s _ hc hf
    omega
  | succ f ihf =>
    intro env e s hInv hc hf
    rw [evalE]
    split
    · rename_i er s1 hch
      exact nf_pair (nf_err charge_nf hch)
    · rename_i u s1 hch
      have och : Out B s ((Except.ok u, s1) : Res Unit) := hch ▸ charge_out 1 hInv
      have hInv1 : Inv B s1 := och.inv
      obtain ⟨hcnt, hc1⟩ := charge_ok hch
      have hf1 : B + 2 ≤ f + s1.count := by omega
      have He := fun env e s h => eval_out B D σ f env e s h
      split
      · -- evalDepth guard tripped (RM-09): a defined error, distinct from `Err.fuel`.
        exact nf_pair (fun hh => nomatch hh)
      · -- evalDepth guard passed. `evalDepth` is invisible to `NF` (it only
        -- inspects the outcome, `r.1`), so the entry bump / exit undo around
        -- the recursive dispatch needs no bridging lemma here — every goal
        -- below is defeq to its unwrapped form. Only the *state* fed to the
        -- recursive `evalE`/`He`/`ihf` calls changes, from `s1` to `s1'`
        -- (`count`/`work`/`effects` identical, so `hInv1'`/`hc1'`/`hf1'` are
        -- the same facts as `hInv1`/`hc1`/`hf1`).
        rename_i hdepth
        let s1' := { s1 with evalDepth := s1.evalDepth + 1 }
        have hInv1' : Inv B s1' := hInv1
        have hc1' : s1'.count ≤ B := hc1
        have hf1' : B + 2 ≤ f + s1'.count := hf1
        refine nf_setDepth _ ?_
        split
        · -- lit
          exact nf_ok
        · -- var
          rename_i sym
          split
          · exact nf_ok
          · split
            · exact nf_pair (fun hh => nomatch hh)
            · rename_i v heq2
              split
              · exact nf_pair (fun hh => nomatch hh)
              · exact nf_ok
        · -- binop
          rename_i op l r
          split
          · rename_i er s2 heq
            exact nf_pair (nf_err (ihf env l s1' hInv1' hc1' hf1') heq)
          · rename_i lv s2 heq
            have o1 : Out B s1' ((Except.ok lv, s2) : Res Val) := heq ▸ He env l s1' hInv1'
            have hc2 : s2.count ≤ B := o1.okb lv rfl hc1'
            have hm1 : s1'.count ≤ s2.count := o1.mono
            split
            · rename_i er s3 heq2
              exact nf_pair (nf_err (ihf env r s2 o1.inv hc2 (by omega)) heq2)
            · rename_i rv s3 heq2
              split
              · rename_i er2 s4 heq3
                exact nf_pair (nf_err charge_nf heq3)
              · rename_i u s4 heq3
                split
                · exact nf_ok
                · rename_i er heq4
                  have hne : er ≠ Err.fuel := fun he =>
                    applyBinop_nf op lv rv (by rw [heq4, he])
                  exact nf_pair hne
        · -- not
          rename_i inner
          split
          · rename_i er s2 heq
            exact nf_pair (nf_err (ihf env inner s1' hInv1' hc1' hf1') heq)
          · rename_i v s2 heq
            split
            · exact nf_ok
            · exact nf_pair (fun hh => nomatch hh)
        · -- ite
          rename_i c t el
          split
          · rename_i er s2 heq
            exact nf_pair (nf_err (ihf env c s1' hInv1' hc1' hf1') heq)
          · rename_i v s2 heq
            have o1 : Out B s1' ((Except.ok v, s2) : Res Val) := heq ▸ He env c s1' hInv1'
            have hc2 : s2.count ≤ B := o1.okb v rfl hc1'
            have hm1 : s1'.count ≤ s2.count := o1.mono
            split
            · exact ihf env t s2 o1.inv hc2 (by omega)
            · exact ihf env el s2 o1.inv hc2 (by omega)
            · exact nf_pair (fun hh => nomatch hh)
        · -- field
          rename_i base fname
          split
          · rename_i er s2 heq
            exact nf_pair (nf_err (ihf env base s1' hInv1' hc1' hf1') heq)
          · rename_i v s2 heq
            split
            · rename_i fs
              split
              · exact nf_ok
              · exact nf_pair (fun hh => nomatch hh)
            · exact nf_pair (fun hh => nomatch hh)
        · -- letE
          rename_i name v body
          split
          · rename_i er s2 heq
            exact nf_pair (nf_err (ihf env v s1' hInv1' hc1' hf1') heq)
          · rename_i bv s2 heq
            have o1 : Out B s1' ((Except.ok bv, s2) : Res Val) := heq ▸ He env v s1' hInv1'
            have hc2 : s2.count ≤ B := o1.okb bv rfl hc1'
            have hm1 : s1'.count ≤ s2.count := o1.mono
            exact ihf ((name, bv) :: env) body s2 o1.inv hc2 (by omega)
        · -- call
          rename_i fname args
          exact evalCall_nf He ihf env fname args s1' hInv1' hc1' hf1'

/-! ## Action-level and reaction-level bounds -/

theorem inv_init {B : Nat} : Inv B EvalSt.init :=
  ⟨Nat.le_refl _, Nat.zero_le _⟩

theorem some_ne_fuel {er : Err} (h : er ≠ Err.fuel) : some er ≠ some Err.fuel := by
  intro hc
  injection hc with h2
  exact h h2

theorem execNetPath_work (B : Nat) (D : Doc) (σ : Store) (m : Method) (alias : Symbol)
    (pv : Option Val) (pathp : Option Expr) (target : Symbol) (s : EvalSt) (h : Inv B s) :
    (execNetPath B D σ m alias pv pathp target s).work ≤ B := by
  rw [execNetPath.eq_def]
  split
  · exact h.2
  · rename_i pp
    split
    · rename_i er s' heq
      exact (show Out B s ((Except.error er, s') : Res Val) from
        heq ▸ eval_out B D σ (B + 2) [] pp s h).wb
    · rename_i v s' heq
      have o : Out B s ((Except.ok v, s') : Res Val) :=
        heq ▸ eval_out B D σ (B + 2) [] pp s h
      split
      · rename_i str
        split
        · exact o.wb
        · exact o.wb
      · rename_i n
        split
        · exact o.wb
        · exact o.wb
      · exact o.wb

theorem execNetPath_nf (B : Nat) (D : Doc) (σ : Store) (m : Method) (alias : Symbol)
    (pv : Option Val) (pathp : Option Expr) (target : Symbol) (s : EvalSt) (h : Inv B s)
    (hc : s.count ≤ B) :
    (execNetPath B D σ m alias pv pathp target s).err ≠ some Err.fuel := by
  rw [execNetPath.eq_def]
  split
  · nofun
  · rename_i pp
    split
    · rename_i er s' heq
      exact some_ne_fuel (nf_err (eval_nf B D σ (B + 2) [] pp s h hc (by omega)) heq)
    · rename_i v s' heq
      split
      · rename_i str
        split
        · nofun
        · exact some_ne_fuel (fun hh => nomatch hh)
      · rename_i n
        split
        · nofun
        · exact some_ne_fuel (fun hh => nomatch hh)
      · exact some_ne_fuel (fun hh => nomatch hh)

theorem execAction_work (B : Nat) (D : Doc) (σ : Store) (a : Action) :
    (execAction B D σ a).work ≤ B := by
  rw [execAction.eq_def]
  split
  · -- eval
    rename_i e
    split <;>
      (have hw := eval_work_le B D σ (B + 2) [] e EvalSt.init inv_init;
       simp only [*] at hw;
       simpa using hw)
  · -- assign
    rename_i t e
    split
    · simp
    · split <;>
        (have hw := eval_work_le B D σ (B + 2) [] e EvalSt.init inv_init;
         simp only [*] at hw;
         simpa using hw)
  · -- navigate
    rename_i url
    split <;>
      (have hw := eval_work_le B D σ (B + 2) [] url EvalSt.init inv_init;
       simp only [*] at hw;
       simpa using hw)
  · -- networkCall
    rename_i m alias payload pathp target
    split
    · exact execNetPath_work B D σ m alias none pathp target EvalSt.init inv_init
    · rename_i p
      split
      · have hw := eval_work_le B D σ (B + 2) [] p EvalSt.init inv_init
        simp only [*] at hw
        simpa using hw
      · have o := eval_out B D σ (B + 2) [] p EvalSt.init inv_init
        simp only [*] at o
        exact execNetPath_work B D σ m alias _ pathp target _ o.inv

theorem execAction_nf (B : Nat) (D : Doc) (σ : Store) (a : Action) :
    (execAction B D σ a).err ≠ some Err.fuel := by
  rw [execAction.eq_def]
  split
  · -- eval
    rename_i e
    split
    · simp
    · have hnf := eval_nf B D σ (B + 2) [] e EvalSt.init inv_init (Nat.zero_le B) (by omega)
      simp only [*] at hnf
      simpa using hnf
  · -- assign
    rename_i t e
    split
    · simp
    · split
      · simp
      · have hnf := eval_nf B D σ (B + 2) [] e EvalSt.init inv_init (Nat.zero_le B) (by omega)
        simp only [*] at hnf
        simpa using hnf
  · -- navigate
    rename_i url
    split
    · simp
    · simp
    · have hnf := eval_nf B D σ (B + 2) [] url EvalSt.init inv_init (Nat.zero_le B) (by omega)
      simp only [*] at hnf
      simpa using hnf
  · -- networkCall
    rename_i m alias payload pathp target
    split
    · exact execNetPath_nf B D σ m alias none pathp target EvalSt.init inv_init (Nat.zero_le B)
    · rename_i p
      split
      · have hnf := eval_nf B D σ (B + 2) [] p EvalSt.init inv_init (Nat.zero_le B) (by omega)
        simp only [*] at hnf
        simpa using hnf
      · have o := eval_out B D σ (B + 2) [] p EvalSt.init inv_init
        simp only [*] at o
        exact execNetPath_nf B D σ m alias _ pathp target _ o.inv (o.okb _ rfl (Nat.zero_le B))

/-! ### Recomputation -/

theorem recomputeStep_work (B : Nat) (D : Doc) (acc : RecompRes) (c : CompDef) :
    (recomputeStep B D acc c).work ≤ acc.work + B := by
  unfold recomputeStep
  split
  · split
    · rename_i v s heq
      have o : Out B EvalSt.init ((Except.ok v, s) : Res Val) :=
        heq ▸ eval_out B D acc.σ (B + 2) [] c.expr EvalSt.init inv_init
      have hw : s.work ≤ B := o.wb
      show acc.work + s.work ≤ acc.work + B
      omega
    · rename_i er s heq
      have o : Out B EvalSt.init ((Except.error er, s) : Res Val) :=
        heq ▸ eval_out B D acc.σ (B + 2) [] c.expr EvalSt.init inv_init
      have hw : s.work ≤ B := o.wb
      show acc.work + s.work ≤ acc.work + B
      omega
  · exact Nat.le_add_right _ _

theorem foldl_recompute_work (B : Nat) (D : Doc) :
    ∀ (l : List CompDef) (acc : RecompRes),
      (l.foldl (recomputeStep B D) acc).work ≤ acc.work + l.length * B := by
  intro l
  induction l with
  | nil =>
    intro acc
    simp
  | cons c rest ih =>
    intro acc
    have h1 := recomputeStep_work B D acc c
    have h2 := ih (recomputeStep B D acc c)
    simp only [List.foldl, List.length_cons]
    rw [Nat.succ_mul]
    omega

theorem recompute_work (B : Nat) (D : Doc) (σ : Store) (ch : List Symbol) :
    (recompute B D σ ch).work ≤ D.comps.length * B := by
  have h := foldl_recompute_work B D D.comps ⟨σ, [], 0, ch, []⟩
  unfold recompute
  simpa using h

theorem recomputeStep_nf (B : Nat) (D : Doc) (acc : RecompRes) (c : CompDef)
    (h : ∀ er ∈ acc.errs, er ≠ Err.fuel) :
    ∀ er ∈ (recomputeStep B D acc c).errs, er ≠ Err.fuel := by
  unfold recomputeStep
  split
  · split
    · rename_i v s heq
      exact h
    · rename_i er s heq
      intro x hx
      have hne : er ≠ Err.fuel :=
        nf_err (eval_nf B D acc.σ (B + 2) [] c.expr EvalSt.init inv_init
          (Nat.zero_le B) (by omega)) heq
      rcases List.mem_append.mp hx with h1 | h1
      · exact h x h1
      · simp at h1
        subst h1
        exact hne
  · exact h

theorem foldl_recompute_nf (B : Nat) (D : Doc) :
    ∀ (l : List CompDef) (acc : RecompRes), (∀ er ∈ acc.errs, er ≠ Err.fuel) →
      ∀ er ∈ (l.foldl (recomputeStep B D) acc).errs, er ≠ Err.fuel := by
  intro l
  induction l with
  | nil => intro acc h; exact h
  | cons c rest ih =>
    intro acc h
    exact ih _ (recomputeStep_nf B D acc c h)

/-- Comp recomputation never surfaces the fuel artifact. -/
theorem recompute_nf (B : Nat) (D : Doc) (σ : Store) (ch : List Symbol) :
    ∀ er ∈ (recompute B D σ ch).errs, er ≠ Err.fuel := by
  unfold recompute
  exact foldl_recompute_nf B D D.comps _ (fun er h => nomatch h)

/-- Comp recomputation emits only navigation-free effects (comps are
expressions; `navigate` is not expression-reachable). -/
theorem recomputeStep_navFree (B : Nat) (D : Doc) (acc : RecompRes) (c : CompDef)
    (h : navFree acc.effects) :
    navFree (recomputeStep B D acc c).effects := by
  unfold recomputeStep
  split
  · split
    · rename_i v s heq
      have o : Out B EvalSt.init ((Except.ok v, s) : Res Val) :=
        heq ▸ eval_out B D acc.σ (B + 2) [] c.expr EvalSt.init inv_init
      obtain ⟨d, e, n⟩ := o.eff
      have e' : s.effects = d := by simpa [EvalSt.init] using e
      show navFree (acc.effects ++ s.effects)
      rw [e']
      exact navFree_append h n
    · rename_i er s heq
      have o : Out B EvalSt.init ((Except.error er, s) : Res Val) :=
        heq ▸ eval_out B D acc.σ (B + 2) [] c.expr EvalSt.init inv_init
      obtain ⟨d, e, n⟩ := o.eff
      have e' : s.effects = d := by simpa [EvalSt.init] using e
      show navFree (acc.effects ++ s.effects)
      rw [e']
      exact navFree_append h n
  · exact h

theorem foldl_recompute_navFree (B : Nat) (D : Doc) :
    ∀ (l : List CompDef) (acc : RecompRes), navFree acc.effects →
      navFree ((l.foldl (recomputeStep B D) acc)).effects := by
  intro l
  induction l with
  | nil => intro acc h; exact h
  | cons c rest ih =>
    intro acc h
    exact ih _ (recomputeStep_navFree B D acc c h)

theorem recompute_navFree (B : Nat) (D : Doc) (σ : Store) (ch : List Symbol) :
    navFree (recompute B D σ ch).effects := by
  unfold recompute
  exact foldl_recompute_navFree B D D.comps _ navFree_nil

/-! ### Reactions and T1 -/

theorem fireTx_work (B : Nat) (D : Doc) (σ : Store) (c : Ctx) (a : Action) :
    (fireTx B D σ c a).work ≤ (1 + D.comps.length) * B := by
  unfold fireTx
  split
  · rename_i x1 x2 x3 w x5 heq
    have hw : w ≤ B := by
      have h := execAction_work B D σ a
      rw [heq] at h
      exact h
    show w ≤ (1 + D.comps.length) * B
    rw [Nat.add_mul, Nat.one_mul]
    omega
  · rename_i σ' effs w ch heq
    have hw : w ≤ B := by
      have h := execAction_work B D σ a
      rw [heq] at h
      exact h
    have hr := recompute_work B D σ' ch
    show w + (recompute B D σ' ch).work ≤ (1 + D.comps.length) * B
    rw [Nat.add_mul, Nat.one_mul]
    omega

theorem fireSubmit_work (B : Nat) (D : Doc) (σ0 : Store) (formSym : Symbol) (a : Action) :
    (fireSubmit B D σ0 formSym a).work ≤ (1 + D.comps.length) * B := by
  unfold fireSubmit
  split
  · rename_i x1 x2 effs w x5 heq
    have hw : w ≤ B := by
      have h := execAction_work B D σ0 a
      rw [heq] at h
      exact h
    have hr := recompute_work B D σ0 [formSym]
    show w + (recompute B D σ0 [formSym]).work ≤ (1 + D.comps.length) * B
    rw [Nat.add_mul, Nat.one_mul]
    omega
  · rename_i σ' effs w ch heq
    have hw : w ≤ B := by
      have h := execAction_work B D σ0 a
      rw [heq] at h
      exact h
    have hr := recompute_work B D σ' (formSym :: ch)
    show w + (recompute B D σ' (formSym :: ch)).work ≤ (1 + D.comps.length) * B
    rw [Nat.add_mul, Nat.one_mul]
    omega

/-- **Expression-work bound for a whole reaction**: at most
`(1 + #comps) · B` — one fresh budget for the action, one per comp binding.
The factor is a static property of the *document*; no input datum enlarges
it. -/
theorem reaction_work_le (B : Nat) (D : Doc) (σ : Store) (ev : Event) :
    (reaction B D σ ev).work ≤ (1 + D.comps.length) * B := by
  cases ev with
  | click i =>
    rw [reaction]
    split
    · exact Nat.zero_le _
    · rename_i a heq
      exact fireTx_work B D σ .gesture a
  | timer i =>
    rw [reaction]
    split
    · exact Nat.zero_le _
    · rename_i a heq
      exact fireTx_work B D σ .nonInteractive a
  | submit i fields =>
    rw [reaction]
    split
    · have h := recompute_work B D (setVar σ D.formSym (.record fields)) [D.formSym]
      show (recompute B D (setVar σ D.formSym (.record fields)) [D.formSym]).work
        ≤ (1 + D.comps.length) * B
      rw [Nat.add_mul, Nat.one_mul]
      omega
    · rename_i a heq
      exact fireSubmit_work B D (setVar σ D.formSym (.record fields)) D.formSym a
  | netResponse t v =>
    rw [reaction]
    split
    · have h := recompute_work B D (setVar σ t v) [t]
      show (recompute B D (setVar σ t v) [t]).work ≤ (1 + D.comps.length) * B
      rw [Nat.add_mul, Nat.one_mul]
      omega
    · exact Nat.zero_le _

/-- **T1 — Bounded reaction.**  One reaction's expression work plus one
frame's layout expansion is bounded by `(1 + #comps) · B + N` — a quantity
determined by the document and the two budget constants alone, independent
of the size of any remote datum.  This is the theorem the manifesto's
"it can't freeze" actually needs, and it is true *only because of* the
budgets: strip `charge` from the semantics and `each`-amplification gives
unbounded acyclic work. -/
theorem T1_reaction_bound (B N : Nat) (D : Doc) (σ : Store) (ev : Event) :
    (reaction B D σ ev).work + expandLayout N D (reaction B D σ ev).σ
      ≤ (1 + D.comps.length) * B + N :=
  Nat.add_le_add (reaction_work_le B D σ ev) (expandLayout_le N D _)

/-- The shipped budgets (`types.rs::MAX_INSTRUCTIONS`,
`layout_bridge.rs::MAX_SYNTHETIC_LAYOUT_NODES`). -/
def MAX_INSTRUCTIONS : Nat := 20000

def MAX_SYNTHETIC_LAYOUT_NODES : Nat := 20000

/-- T1 at the shipped constants. -/
theorem T1_shipped (D : Doc) (σ : Store) (ev : Event) :
    (reaction MAX_INSTRUCTIONS D σ ev).work
      + expandLayout MAX_SYNTHETIC_LAYOUT_NODES D (reaction MAX_INSTRUCTIONS D σ ev).σ
      ≤ (1 + D.comps.length) * 20000 + 20000 :=
  T1_reaction_bound MAX_INSTRUCTIONS MAX_SYNTHETIC_LAYOUT_NODES D σ ev

/-! ## RM-08 — the comp-count cap

`T1_shipped` bounds one reaction by `(1 + D.comps.length) * 20000 + 20000` —
an honest bound, but still a function of the *document*: nothing in the
model up to this point stops `D.comps.length` from being arbitrarily large,
so the bound above is not by itself a fixed number. A document declaring,
say, 5000 `comp` bindings that all transitively depend on one mutable
variable is priced correctly by `T1_shipped` (`(1 + 5000) * 20000`
instructions), but that price is a very different number from what
`MAX_INSTRUCTIONS = 20000` suggests in isolation, and is a practical DoS
vector for a document loaded from an untrusted origin.

`MAX_COMP_BINDINGS` mirrors the cap enforced on the Rust side, at parse time,
in `parser::logic::parse_computed_with_functions` (`src/core/types.rs`): a
document declaring more `comp` bindings than this is rejected with a
`ParseError` before it ever reaches the evaluator, so every `Doc` that
*successfully loads* satisfies `D.comps.length ≤ MAX_COMP_BINDINGS`.
`T1_shipped_capped` composes that load-time invariant with `T1_shipped` to
produce a bound that depends on nothing but the three shipped constants —
the fully concrete number `MAX_INSTRUCTIONS = 20000` alone was always
implicitly promising. -/

/-- The comp-count cap enforced at parse time (`types.rs::MAX_COMP_BINDINGS`).
Kept in sync manually with the Rust constant; see `RM-08` in `walkthrough.md`
at the repository root. -/
def MAX_COMP_BINDINGS : Nat := 500

set_option maxRecDepth 65536

/-- **T1, capped.** For any document that passed the `MAX_COMP_BINDINGS`
load-time check (i.e. any document the Rust parser actually accepts), one
reaction's expression work plus one frame's layout expansion is bounded by a
single concrete number depending only on the three shipped budget constants
— never on the document, and never on the size of any remote datum. This is
the corollary the comp-count cap exists to establish. -/
theorem T1_shipped_capped (D : Doc) (σ : Store) (ev : Event)
    (hcap : D.comps.length ≤ MAX_COMP_BINDINGS) :
    (reaction MAX_INSTRUCTIONS D σ ev).work
      + expandLayout MAX_SYNTHETIC_LAYOUT_NODES D (reaction MAX_INSTRUCTIONS D σ ev).σ
      ≤ (1 + MAX_COMP_BINDINGS) * 20000 + 20000 := by
  have h := T1_shipped D σ ev
  have hcap' : D.comps.length ≤ 500 := hcap
  omega

end Mizu
