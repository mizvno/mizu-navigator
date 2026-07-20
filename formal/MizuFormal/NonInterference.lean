import MizuFormal.Flow
import MizuFormal.Budget

/-!
# T2 — Flow-checker soundness as non-interference

For an accepted document, the destinations reached **without a gate** are
independent of untrusted input.  Two runs that differ only in the values
delivered by untrusted sources — network responses (`netResponse`) and form
fields (`submit`) — produce the *same* sequence of ungated navigation
requests, and their stores stay equal on every untainted symbol.

The proof is a bisimulation whose relation is `lowEq (taintOf D)`:

* `eval_agree` — evaluation of an *untainted* expression cannot observe the
  stores' tainted part: results (value, cost, effects) are identical.
  This is where the checker's structural taint propagation
  (`isTaintedE`) meets the operational semantics clause by clause.
* Comp recomputation preserves the relation (`recomputeStep_agree`):
  untainted comps recompute identically (their static dependency sets are
  untainted — `collectReads_unt`); tainted comps may diverge but only ever
  write tainted names.
* Actions preserve the relation; the only ungated navigation source is a
  non-interactive `Action.navigate`, whose URL expression `accept` certifies
  untainted (`accept_navigate`), so both runs emit the same request — the
  per-action budget reset and the transactional rollback make even the
  *timing* of that emission identical, closing the termination channel for
  ungated navigations.
-/

namespace Mizu

/-! ## Low-equivalence -/

/-- Stores agree on every untainted symbol (presence and value). -/
def lowEq (T : Taint) (σ₁ σ₂ : Store) : Prop :=
  ∀ s, T.vars.contains s = false → alookup σ₁ s = alookup σ₂ s

theorem lowEq.refl (T : Taint) (σ : Store) : lowEq T σ σ := fun _ _ => rfl

theorem lowEq.symm {T : Taint} {σ₁ σ₂ : Store} (h : lowEq T σ₁ σ₂) : lowEq T σ₂ σ₁ :=
  fun s hs => (h s hs).symm

theorem lowEq.trans {T : Taint} {σ₁ σ₂ σ₃ : Store}
    (h1 : lowEq T σ₁ σ₂) (h2 : lowEq T σ₂ σ₃) : lowEq T σ₁ σ₃ :=
  fun s hs => (h1 s hs).trans (h2 s hs)

/-- Writing the *same* value to the same symbol preserves low-equivalence. -/
theorem lowEq_setVar_same {T : Taint} {σ₁ σ₂ : Store} (h : lowEq T σ₁ σ₂)
    (t : Symbol) (v : Val) : lowEq T (setVar σ₁ t v) (setVar σ₂ t v) := by
  intro s hs
  show alookup ((t, v) :: σ₁) s = alookup ((t, v) :: σ₂) s
  rw [alookup, alookup]
  split
  · rfl
  · exact h s hs

/-- Writing (possibly different values) to a *tainted* symbol preserves
low-equivalence. -/
theorem lowEq_setVar_tainted {T : Taint} {σ₁ σ₂ : Store} (h : lowEq T σ₁ σ₂)
    {t : Symbol} (ht : T.vars.contains t = true) (v₁ v₂ : Val) :
    lowEq T (setVar σ₁ t v₁) (setVar σ₂ t v₂) := by
  intro s hs
  have hne : (t == s) = false := by
    cases hbeq : t == s
    · rfl
    · exfalso
      have : t = s := by simpa using hbeq
      subst this
      rw [ht] at hs
      cases hs
  show alookup ((t, v₁) :: σ₁) s = alookup ((t, v₂) :: σ₂) s
  rw [alookup, alookup, hne]
  exact h s hs

/-- Single-run frame: writing a tainted symbol is invisible at low level. -/
theorem lowEq_setVar_frame {T : Taint} {σ : Store} {t : Symbol}
    (ht : T.vars.contains t = true) (v : Val) : lowEq T σ (setVar σ t v) := by
  intro s hs
  have hne : (t == s) = false := by
    cases hbeq : t == s
    · rfl
    · exfalso
      have : t = s := by simpa using hbeq
      subst this
      rw [ht] at hs
      cases hs
  show alookup σ s = alookup ((t, v) :: σ) s
  simp [alookup, hne]

/-- `σ'` is either `σ` unchanged, or `σ` with `t` freshly written to some value —
the two shapes `execAction`'s `assign` arm and `recomputeStep` can produce for
their write-target symbol. -/
def OptWrite (σ σ' : Store) (t : Symbol) : Prop := σ' = σ ∨ ∃ v, σ' = setVar σ t v

/-- Two low-equivalent stores, each optionally (and independently) written at
the *same tainted* symbol with *arbitrary, possibly different* values, stay
low-equivalent.  This is the single fact that lets a tainted `comp`/`assign`
diverge freely between the two runs — in outcome, in value, even in whether it
fires at all — without disturbing the untainted part of the store. -/
theorem lowEq_write_opt {T : Taint} {σ₁ σ₂ σ₁' σ₂' : Store} {t : Symbol}
    (h : lowEq T σ₁ σ₂) (ht : T.vars.contains t = true)
    (h1 : OptWrite σ₁ σ₁' t) (h2 : OptWrite σ₂ σ₂' t) :
    lowEq T σ₁' σ₂' := by
  rcases h1 with h1 | ⟨v1, h1⟩ <;> rcases h2 with h2 | ⟨v2, h2⟩ <;> subst h1 <;> subst h2
  · exact h
  · exact h.trans (lowEq_setVar_frame ht v2)
  · exact (h.symm.trans (lowEq_setVar_frame ht v1)).symm
  · exact lowEq_setVar_tainted h ht v1 v2

/-! ## Changed-set agreement

`recompute`'s trigger condition for a comp — and, transitively, which further
comps cascade — is a Boolean test against the accumulator's `changed : List
Symbol`.  For the fold to stay lockstep between two low-equivalent runs, the
`changed` lists don't need to be *equal* (a tainted comp may or may not fire,
independently, in each run) — they only need to agree on **untainted**
symbols, exactly mirroring `lowEq` itself one level up. -/

/-- `changed`-list agreement: two symbol lists agree on membership for every
untainted symbol.  Tainted symbols may be present in one list and absent from
the other. -/
def chAgree (T : Taint) (ch1 ch2 : List Symbol) : Prop :=
  ∀ s, T.vars.contains s = false → ch1.contains s = ch2.contains s

theorem chAgree.refl (T : Taint) (ch : List Symbol) : chAgree T ch ch := fun _ _ => rfl

theorem chAgree.symm {T : Taint} {ch1 ch2 : List Symbol} (h : chAgree T ch1 ch2) :
    chAgree T ch2 ch1 := fun s hs => (h s hs).symm

/-- Prepending the *same* symbol to both lists preserves agreement — no
taintedness needed: the shared symbol contributes the same Boolean to both
sides' `contains` regardless of its own taint status. -/
theorem chAgree_cons_same {T : Taint} {ch1 ch2 : List Symbol} (h : chAgree T ch1 ch2)
    (c : Symbol) : chAgree T (c :: ch1) (c :: ch2) := by
  intro s hs
  rw [List.contains_cons, List.contains_cons, h s hs]

theorem chAgree_cons_tainted_left {T : Taint} {ch1 ch2 : List Symbol} (h : chAgree T ch1 ch2)
    {t : Symbol} (ht : T.vars.contains t = true) : chAgree T (t :: ch1) ch2 := by
  intro s hs
  have hne : (s == t) = false := by
    cases hbeq : s == t
    · rfl
    · exfalso
      have : s = t := by simpa using hbeq
      subst this
      rw [ht] at hs
      cases hs
  rw [List.contains_cons, hne, Bool.false_or]
  exact h s hs

theorem chAgree_cons_tainted_right {T : Taint} {ch1 ch2 : List Symbol} (h : chAgree T ch1 ch2)
    {t : Symbol} (ht : T.vars.contains t = true) : chAgree T ch1 (t :: ch2) := by
  intro s hs
  have hne : (s == t) = false := by
    cases hbeq : s == t
    · rfl
    · exfalso
      have : s = t := by simpa using hbeq
      subst this
      rw [ht] at hs
      cases hs
  rw [List.contains_cons, hne, Bool.false_or]
  exact h s hs

/-- `l'` is either `l` unchanged, or `l` with `t` freshly prepended — the two
shapes `execAction`'s `assign` arm and `recomputeStep` can produce for their
`changed`/output list. -/
def OptCons (l l' : List Symbol) (t : Symbol) : Prop := l' = l ∨ l' = t :: l

/-- The `chAgree` analogue of `lowEq_write_opt`: independently, possibly
divergent, prepending of the *same tainted* symbol to each side preserves
agreement. -/
theorem chAgree_write_opt {T : Taint} {ch1 ch2 ch1' ch2' : List Symbol} {t : Symbol}
    (h : chAgree T ch1 ch2) (ht : T.vars.contains t = true)
    (h1 : OptCons ch1 ch1' t) (h2 : OptCons ch2 ch2' t) :
    chAgree T ch1' ch2' := by
  rcases h1 with h1 | h1 <;> rcases h2 with h2 | h2 <;> subst h1 <;> subst h2
  · exact h
  · exact chAgree_cons_tainted_right h ht
  · exact chAgree_cons_tainted_left h ht
  · exact chAgree_cons_same h t

/-- If two `changed` lists agree on every untainted symbol, then `List.any`
of a *membership test against an all-untainted list* — exactly the shape of a
comp's trigger check when the comp itself is untainted (`collectReads_unt`) —
gives the same Boolean against either list. -/
theorem any_contains_agree {l : List Symbol} {ch1 ch2 : List Symbol}
    (h : ∀ d ∈ l, ch1.contains d = ch2.contains d) :
    l.any (fun d => ch1.contains d) = l.any (fun d => ch2.contains d) := by
  induction l with
  | nil => rfl
  | cons a rest ih =>
    simp only [List.any_cons]
    rw [h a (List.mem_cons_self ..), ih (fun d hd => h d (List.mem_cons_of_mem _ hd))]

/-! ## Trace projections -/

/-- Navigation URLs in a raw effect list. -/
def navsOfEffects (effs : List Effect) : List String :=
  effs.filterMap fun e =>
    match e with
    | .navigate u => some u
    | _ => none

theorem navsOfEffects_navFree {effs : List Effect} (h : navFree effs) :
    navsOfEffects effs = [] := by
  induction effs with
  | nil => rfl
  | cons e rest ih =>
    have he : e.isNav = false := h e (List.mem_cons_self ..)
    have hrest : navFree rest := fun x hx => h x (List.mem_cons_of_mem _ hx)
    have ihr : navsOfEffects rest = [] := ih hrest
    unfold navsOfEffects at ihr ⊢
    cases e <;> simp_all [Effect.isNav]

theorem navsOfEffects_append (a b : List Effect) :
    navsOfEffects (a ++ b) = navsOfEffects a ++ navsOfEffects b := by
  simp [navsOfEffects, List.filterMap_append]

theorem ungatedNavs_append (a b : List (Ctx × Effect)) :
    ungatedNavs (a ++ b) = ungatedNavs a ++ ungatedNavs b := by
  simp [ungatedNavs, List.filterMap_append]

/-- Gesture-tagged effects contribute nothing to the ungated projection —
this *is* gate G1 in the trace algebra. -/
theorem ungatedNavs_tag_gesture (effs : List Effect) :
    ungatedNavs (tag Ctx.gesture effs) = [] := by
  induction effs with
  | nil => rfl
  | cons e rest ih =>
    cases e <;> simpa [ungatedNavs, tag] using ih

/-- Non-interactive effects project to their navigation URLs. -/
theorem ungatedNavs_tag_nonInteractive (effs : List Effect) :
    ungatedNavs (tag Ctx.nonInteractive effs) = navsOfEffects effs := by
  induction effs with
  | nil => rfl
  | cons e rest ih =>
    cases e <;> simpa [ungatedNavs, tag, navsOfEffects] using ih

theorem ungatedNavs_nil : ungatedNavs [] = [] := rfl

/-- Effects of an evaluation started from a fresh state are navigation-free
(the `navigate` capability is not expression-reachable). -/
theorem eval_init_navFree (B : Nat) (D : Doc) (σ : Store) (fuel : Nat) (env : Env)
    (e : Expr) : navFree (evalE B D σ fuel env e EvalSt.init).2.effects := by
  obtain ⟨d, he, hn⟩ := (eval_out B D σ fuel env e EvalSt.init inv_init).eff
  rw [he]
  intro x hx
  rcases List.mem_append.mp hx with hx | hx
  · simp [EvalSt.init] at hx
  · exact hn x hx

/-! ## Evaluation agreement

An untainted expression evaluates identically against low-equivalent stores:
same outcome, same cost, same effects.  Modular in the same style as
`Budget.eval_out`. -/

section Agree

variable {B : Nat} {D : Doc} {T : Taint} {σ₁ σ₂ : Store} {fuel : Nat}

theorem evalArgs_agree
    (Hag : ∀ env e s, isTaintedE T e = false →
      evalE B D σ₁ fuel env e s = evalE B D σ₂ fuel env e s) :
    ∀ (args : List Expr) (env : Env) (s : EvalSt), isTaintedL T args = false →
      evalArgs B D σ₁ fuel env args s = evalArgs B D σ₂ fuel env args s := by
  intro args
  induction args with
  | nil =>
    intro env s _
    rw [evalArgs, evalArgs]
  | cons a rest ih =>
    intro env s h
    simp only [isTaintedL, Bool.or_eq_false_iff] at h
    rw [evalArgs, evalArgs, Hag env a s h.1]
    split
    · rfl
    · rename_i v s1 _
      rw [ih env s1 h.2]

theorem evalStoreLocal_agree
    (Hag : ∀ env e s, isTaintedE T e = false →
      evalE B D σ₁ fuel env e s = evalE B D σ₂ fuel env e s)
    (env : Env) (k v : Expr) (s : EvalSt)
    (hk : isTaintedE T k = false) (hv : isTaintedE T v = false) :
    evalStoreLocal B D σ₁ fuel env k v s = evalStoreLocal B D σ₂ fuel env k v s := by
  rw [evalStoreLocal, evalStoreLocal, Hag env k s hk]
  split
  · rfl
  · rename_i kv s1 _
    split
    · rw [Hag env v s1 hv]
    · rfl

theorem evalClip_agree
    (Hag : ∀ env e s, isTaintedE T e = false →
      evalE B D σ₁ fuel env e s = evalE B D σ₂ fuel env e s)
    (env : Env) (x : Expr) (s : EvalSt) (hx : isTaintedE T x = false) :
    evalClip B D σ₁ fuel env x s = evalClip B D σ₂ fuel env x s := by
  rw [evalClip, evalClip, Hag env x s hx]

theorem evalFilter_agree
    (Hag : ∀ env e s, isTaintedE T e = false →
      evalE B D σ₁ fuel env e s = evalE B D σ₂ fuel env e s)
    (env : Env) (l f t : Expr) (s : EvalSt)
    (hl : isTaintedE T l = false) (hf : isTaintedE T f = false)
    (ht : isTaintedE T t = false) :
    evalFilter B D σ₁ fuel env l f t s = evalFilter B D σ₂ fuel env l f t s := by
  rw [evalFilter, evalFilter, Hag env l s hl]
  split
  · rfl
  · rename_i lv s1 _
    rw [Hag env f s1 hf]
    split
    · rfl
    · rename_i fv s2 _
      rw [Hag env t s2 ht]

theorem evalCount_agree
    (Hag : ∀ env e s, isTaintedE T e = false →
      evalE B D σ₁ fuel env e s = evalE B D σ₂ fuel env e s)
    (env : Env) (l f t : Expr) (s : EvalSt)
    (hl : isTaintedE T l = false) (hf : isTaintedE T f = false)
    (ht : isTaintedE T t = false) :
    evalCount B D σ₁ fuel env l f t s = evalCount B D σ₂ fuel env l f t s := by
  rw [evalCount, evalCount, Hag env l s hl]
  split
  · rfl
  · rename_i lv s1 _
    rw [Hag env f s1 hf]
    split
    · rfl
    · rename_i fv s2 _
      rw [Hag env t s2 ht]

theorem evalSort_agree
    (Hag : ∀ env e s, isTaintedE T e = false →
      evalE B D σ₁ fuel env e s = evalE B D σ₂ fuel env e s)
    (env : Env) (l f d : Expr) (s : EvalSt)
    (hl : isTaintedE T l = false) (hf : isTaintedE T f = false)
    (hd : isTaintedE T d = false) :
    evalSort B D σ₁ fuel env l f d s = evalSort B D σ₂ fuel env l f d s := by
  rw [evalSort, evalSort, Hag env l s hl]
  split
  · rfl
  · rename_i lv s1 _
    rw [Hag env f s1 hf]
    split
    · rfl
    · rename_i fv s2 _
      rw [Hag env d s2 hd]

theorem evalUser_agree (hfc : FnClosure D T)
    (Hag : ∀ env e s, isTaintedE T e = false →
      evalE B D σ₁ fuel env e s = evalE B D σ₂ fuel env e s)
    (env : Env) (fname : Symbol) (args : List Expr) (s : EvalSt)
    (hfname : T.fns.contains fname = false) (hargs : isTaintedL T args = false) :
    evalUser B D σ₁ fuel env fname args s = evalUser B D σ₂ fuel env fname args s := by
  rw [evalUser, evalUser]
  split
  · rfl
  · rename_i fd heq
    split
    · rw [evalArgs_agree Hag args env s hargs]
      split
      · rfl
      · rename_i vals s1 _
        exact Hag _ fd.body s1 (hfc fname fd heq hfname)
    · rfl

theorem evalCall_agree (hfc : FnClosure D T)
    (Hag : ∀ env e s, isTaintedE T e = false →
      evalE B D σ₁ fuel env e s = evalE B D σ₂ fuel env e s)
    (env : Env) (fname : Symbol) (args : List Expr) (s : EvalSt)
    (hfname : T.fns.contains fname = false) (hargs : isTaintedL T args = false) :
    evalCall B D σ₁ fuel env fname args s = evalCall B D σ₂ fuel env fname args s := by
  rw [evalCall.eq_def, evalCall.eq_def]
  split
  · -- storeLocal
    split
    · rename_i k v
      simp only [isTaintedL, Bool.or_eq_false_iff, Bool.or_false] at hargs
      exact evalStoreLocal_agree Hag env k v s hargs.1 hargs.2
    · rfl
  · -- copyClipboard
    split
    · rename_i x
      simp only [isTaintedL, Bool.or_false] at hargs
      exact evalClip_agree Hag env x s hargs
    · rfl
  · -- download (no store access at all)
    split
    · rfl
    · exact evalUser_agree hfc Hag env fname _ s hfname hargs
  · -- getSystemTime (no store access at all; hard error on arity mismatch —
    -- unlike download/filter/count/sort, it never falls through to
    -- evalUser, so *both* branches are σ-independent and close by `rfl`)
    split <;> rfl
  · -- filter
    split
    · rename_i l f t
      simp only [isTaintedL, Bool.or_eq_false_iff, Bool.or_false] at hargs
      exact evalFilter_agree Hag env l f t s hargs.1 hargs.2.1 hargs.2.2
    · exact evalUser_agree hfc Hag env fname _ s hfname hargs
  · -- count
    split
    · rename_i l f t
      simp only [isTaintedL, Bool.or_eq_false_iff, Bool.or_false] at hargs
      exact evalCount_agree Hag env l f t s hargs.1 hargs.2.1 hargs.2.2
    · exact evalUser_agree hfc Hag env fname _ s hfname hargs
  · -- sort
    split
    · rename_i l f d
      simp only [isTaintedL, Bool.or_eq_false_iff, Bool.or_false] at hargs
      exact evalSort_agree Hag env l f d s hargs.1 hargs.2.1 hargs.2.2
    · exact evalUser_agree hfc Hag env fname _ s hfname hargs
  · exact evalUser_agree hfc Hag env fname args s hfname hargs

end Agree

/-- **Evaluation agreement**: an untainted expression evaluates identically —
same value or error, same instruction cost, same effects — against stores
that agree on the untainted symbols. -/
theorem eval_agree (B : Nat) (D : Doc) (T : Taint) (hfc : FnClosure D T) :
    ∀ (fuel : Nat) (σ₁ σ₂ : Store), lowEq T σ₁ σ₂ →
      ∀ (env : Env) (e : Expr) (s : EvalSt), isTaintedE T e = false →
        evalE B D σ₁ fuel env e s = evalE B D σ₂ fuel env e s := by
  intro fuel
  induction fuel with
  | zero =>
    intro σ₁ σ₂ _ env e s _
    rw [evalE, evalE]
  | succ f ih =>
    intro σ₁ σ₂ hlow env e s h
    have Hag : ∀ env e s, isTaintedE T e = false →
        evalE B D σ₁ f env e s = evalE B D σ₂ f env e s :=
      fun env e s he => ih σ₁ σ₂ hlow env e s he
    rw [evalE, evalE]
    split
    · rfl
    · rename_i u s1 _
      split
      · -- evalDepth guard (RM-09): trips identically on both sides — the
        -- check depends only on `s1.evalDepth`, never on `σ`.
        rfl
      · -- guard passed: recursion proceeds against the depth-bumped state,
        -- shared by both sides for the same reason. `dsimp only` clears the
        -- `have`/`let` wrapping both sides independently introduced when
        -- `rw [evalE, evalE]` unfolded each occurrence, so `generalize` can
        -- abstract the (now literally identical) depth-bumped state on both
        -- sides at once into one shared name usable in `Hag` calls below.
        rename_i hdepth
        dsimp only
        generalize hs1' : ({ s1 with evalDepth := s1.evalDepth + 1 } : EvalSt) = s1'
        split
        · -- lit
          rfl
        · -- var
          rename_i sym
          simp only [isTaintedE] at h
          simp only [hlow sym h]
        · -- binop
          rename_i op l r
          simp only [isTaintedE, Bool.or_eq_false_iff] at h
          rw [Hag env l s1' h.1]
          split
          · rfl
          · rename_i lv s2 _
            rw [Hag env r s2 h.2]
        · -- not
          rename_i inner
          simp only [isTaintedE] at h
          rw [Hag env inner s1' h]
        · -- ite
          rename_i c t el
          simp only [isTaintedE, Bool.or_eq_false_iff] at h
          rw [Hag env c s1' h.1.1]
          split
          · rfl
          · rename_i v s2 _
            split
            · rw [Hag env t s2 h.1.2]
            · rw [Hag env el s2 h.2]
            · rfl
        · -- field
          rename_i base fname
          simp only [isTaintedE] at h
          rw [Hag env base s1' h]
        · -- letE
          rename_i name v body
          simp only [isTaintedE, Bool.or_eq_false_iff] at h
          rw [Hag env v s1' h.1]
          split
          · rfl
          · rename_i bv s2 _
            rw [Hag ((name, bv) :: env) body s2 h.2]
        · -- call
          rename_i fname args
          simp only [isTaintedE, Bool.or_eq_false_iff] at h
          rw [evalCall_agree hfc Hag env fname args s1' h.1 h.2]

/-! ## Comp recomputation agreement

`recomputeStep`'s effect on its accumulator is, on every path, one of exactly
two shapes for the store and the `changed` list: leave them alone, or write
the store at `c.name` and prepend `c.name` to `changed` — `OptWrite`/`OptCons`
capture exactly this. -/

theorem recomputeStep_writes_opt (B : Nat) (D : Doc) (acc : RecompRes) (c : CompDef) :
    OptWrite acc.σ (recomputeStep B D acc c).σ c.name ∧
    OptCons acc.changed (recomputeStep B D acc c).changed c.name := by
  unfold recomputeStep
  split
  · split
    · exact ⟨Or.inr ⟨_, rfl⟩, Or.inr rfl⟩
    · exact ⟨Or.inl rfl, Or.inl rfl⟩
  · exact ⟨Or.inl rfl, Or.inl rfl⟩

/-- **Recomputation agreement**, one comp at a time.  Two cases:

* `c.name` **tainted** — no assumption on `c.expr` is available or needed;
  whatever the two runs compute (possibly different values, possibly one
  errors and the other doesn't, possibly the trigger condition itself differs
  because a tainted dependency is present in one run's `changed` and not the
  other's), the write is confined to the tainted symbol `c.name`, so
  `lowEq_write_opt`/`chAgree_write_opt` close it unconditionally.
* `c.name` **untainted** — `stable_comp` (closure from `Taint.stable`) gives
  an untainted `c.expr`; `collectReads_unt` then gives an all-untainted
  `compDeps`, so the trigger Boolean itself is provably the *same* in both
  runs (`any_contains_agree`); if it fires, `eval_agree` gives *literal*
  equality of the evaluation outcome, so both runs write the same value (or
  fail identically) and the `changed` lists stay in lockstep. -/
theorem recomputeStep_agree (B : Nat) (D : Doc) (T : Taint) (hfc : FnClosure D T)
    (hst : Taint.stable D T = true) {c : CompDef} (hmem : c ∈ D.comps)
    {acc1 acc2 : RecompRes}
    (hlow : lowEq T acc1.σ acc2.σ) (hch : chAgree T acc1.changed acc2.changed) :
    lowEq T (recomputeStep B D acc1 c).σ (recomputeStep B D acc2 c).σ ∧
    chAgree T (recomputeStep B D acc1 c).changed (recomputeStep B D acc2 c).changed := by
  by_cases htc : T.vars.contains c.name = true
  · obtain ⟨hw1, hc1⟩ := recomputeStep_writes_opt B D acc1 c
    obtain ⟨hw2, hc2⟩ := recomputeStep_writes_opt B D acc2 c
    exact ⟨lowEq_write_opt hlow htc hw1 hw2, chAgree_write_opt hch htc hc1 hc2⟩
  · have htc' : T.vars.contains c.name = false := by
      cases h : T.vars.contains c.name
      · rfl
      · exact absurd h htc
    have hexpr : isTaintedE T c.expr = false := stable_comp hst hmem htc'
    have hdeps : ∀ d ∈ compDeps D c, T.vars.contains d = false :=
      collectReads_unt hfc _ c.expr hexpr
    have htrig : (compDeps D c).any (fun d => acc1.changed.contains d)
        = (compDeps D c).any (fun d => acc2.changed.contains d) :=
      any_contains_agree (fun d hd => hch d (hdeps d hd))
    have hev := eval_agree B D T hfc (B + 2) acc1.σ acc2.σ hlow [] c.expr .init hexpr
    unfold recomputeStep
    rw [htrig]
    split
    · rw [hev]
      split
      · rename_i v _ _
        exact ⟨lowEq_setVar_same hlow c.name v, chAgree_cons_same hch c.name⟩
      · exact ⟨hlow, hch⟩
    · exact ⟨hlow, hch⟩

/-- **Fold-level recomputation agreement**, generalised over any prefix of
comps (not just `D.comps`) so induction goes through directly on
`List.foldl`. -/
theorem foldl_recompute_agree (B : Nat) (D : Doc) (T : Taint) (hfc : FnClosure D T)
    (hst : Taint.stable D T = true) :
    ∀ (l : List CompDef), (∀ c ∈ l, c ∈ D.comps) →
      ∀ {acc1 acc2 : RecompRes}, lowEq T acc1.σ acc2.σ → chAgree T acc1.changed acc2.changed →
        lowEq T (l.foldl (recomputeStep B D) acc1).σ (l.foldl (recomputeStep B D) acc2).σ ∧
        chAgree T (l.foldl (recomputeStep B D) acc1).changed
          (l.foldl (recomputeStep B D) acc2).changed := by
  intro l
  induction l with
  | nil =>
    intro _ acc1 acc2 hlow hch
    exact ⟨hlow, hch⟩
  | cons c rest ih =>
    intro hmem acc1 acc2 hlow hch
    have hmemc : c ∈ D.comps := hmem c (List.mem_cons_self ..)
    have hmemrest : ∀ c' ∈ rest, c' ∈ D.comps :=
      fun c' hc' => hmem c' (List.mem_cons_of_mem _ hc')
    obtain ⟨hlow1, hch1⟩ := recomputeStep_agree B D T hfc hst hmemc hlow hch
    simp only [List.foldl_cons]
    exact ih hmemrest hlow1 hch1

/-- **Recomputation agreement** over a whole document's `comps`, in parsed
order — the model's `recompute`. -/
theorem recompute_agree (B : Nat) (D : Doc) (T : Taint) (hfc : FnClosure D T)
    (hst : Taint.stable D T = true) {σ1 σ2 : Store} (hlow : lowEq T σ1 σ2)
    {ch1 ch2 : List Symbol} (hch : chAgree T ch1 ch2) :
    lowEq T (recompute B D σ1 ch1).σ (recompute B D σ2 ch2).σ ∧
    chAgree T (recompute B D σ1 ch1).changed (recompute B D σ2 ch2).changed := by
  unfold recompute
  exact foldl_recompute_agree B D T hfc hst D.comps (fun _ h => h) hlow hch

/-! ## Action agreement

`execAction` writes the store **only** in the `assign` arm, and only ever at
its own (statically fixed) target symbol — `eval`/`navigate`/`networkCall`
never touch `σ` at all.  These single-run structural facts let
`execAction_agree` below dodge exactly the same case analysis
`recomputeStep_agree` needed, without re-deriving it. -/

theorem execAction_eval_no_write (B : Nat) (D : Doc) (σ : Store) (e : Expr) :
    (execAction B D σ (Action.eval e)).σ = σ ∧
    (execAction B D σ (Action.eval e)).changed = [] := by
  unfold execAction
  split <;> exact ⟨rfl, rfl⟩

theorem execAction_navigate_no_write (B : Nat) (D : Doc) (σ : Store) (url : Expr) :
    (execAction B D σ (Action.navigate url)).σ = σ ∧
    (execAction B D σ (Action.navigate url)).changed = [] := by
  unfold execAction
  split <;> exact ⟨rfl, rfl⟩

theorem execNetPath_no_write (B : Nat) (D : Doc) (σ : Store) (m : Method) (alias : Symbol)
    (pv : Option Val) (pathp : Option Expr) (target : Symbol) (s : EvalSt) :
    (execNetPath B D σ m alias pv pathp target s).σ = σ ∧
    (execNetPath B D σ m alias pv pathp target s).changed = [] := by
  unfold execNetPath
  split <;> (try split) <;> (try split) <;> (try split) <;> exact ⟨rfl, rfl⟩

theorem execAction_networkCall_no_write (B : Nat) (D : Doc) (σ : Store) (m : Method)
    (alias : Symbol) (payload pathp : Option Expr) (target : Symbol) :
    (execAction B D σ (Action.networkCall m alias payload pathp target)).σ = σ ∧
    (execAction B D σ (Action.networkCall m alias payload pathp target)).changed = [] := by
  unfold execAction
  split
  · exact execNetPath_no_write B D σ m alias none pathp target EvalSt.init
  · split
    · exact ⟨rfl, rfl⟩
    · exact execNetPath_no_write ..

theorem execAction_assign_compSym_no_write (B : Nat) (D : Doc) (σ : Store) (t : Symbol) (e : Expr)
    (hcs : D.compSyms.contains t = true) :
    (execAction B D σ (Action.assign t e)).σ = σ ∧
    (execAction B D σ (Action.assign t e)).changed = [] := by
  unfold execAction
  split
  · exact ⟨rfl, rfl⟩
  · rename_i hc
    exact absurd hcs hc

theorem execAction_assign_writes_opt (B : Nat) (D : Doc) (σ : Store) (t : Symbol) (e : Expr)
    (hcs : D.compSyms.contains t = false) :
    OptWrite σ (execAction B D σ (Action.assign t e)).σ t ∧
    OptCons [] (execAction B D σ (Action.assign t e)).changed t := by
  unfold execAction
  split
  · rename_i hc
    rw [hcs] at hc
    cases hc
  · split
    · exact ⟨Or.inr ⟨_, rfl⟩, Or.inr rfl⟩
    · exact ⟨Or.inl rfl, Or.inl rfl⟩

/-- **Action agreement**: for an action that actually occurs in the document
(at *some* gate context `ctx` — the specific tag doesn't matter here), running
it against two low-equivalent stores preserves `lowEq` on the resulting store
and `chAgree` on the resulting `changed` list.

No taintedness assumption on `a` itself is needed: an untainted `assign`
target gets *literal* agreement via `stable_assign` + `eval_agree` (both runs
write the same value, or fail identically); a tainted one — or any of
`eval`/`navigate`/`networkCall`, none of which ever write the store — is
closed by the write-confinement lemmas above, regardless of how the two runs'
outcomes diverge.

Effect/trace content is deliberately **not** part of the conclusion; see
`execAction_navFree_or_navigate` and `T2_non_interference`'s scope note. -/
theorem execAction_agree (B : Nat) (D : Doc) (T : Taint) (hfc : FnClosure D T)
    (hst : Taint.stable D T = true) {ctx : Ctx} {a : Action} (hmem : (ctx, a) ∈ allActions D)
    {σ1 σ2 : Store} (hlow : lowEq T σ1 σ2) :
    lowEq T (execAction B D σ1 a).σ (execAction B D σ2 a).σ ∧
    chAgree T (execAction B D σ1 a).changed (execAction B D σ2 a).changed := by
  cases a with
  | eval e =>
    obtain ⟨hσ1, hc1⟩ := execAction_eval_no_write B D σ1 e
    obtain ⟨hσ2, hc2⟩ := execAction_eval_no_write B D σ2 e
    rw [hσ1, hσ2, hc1, hc2]
    exact ⟨hlow, chAgree.refl T []⟩
  | navigate url =>
    obtain ⟨hσ1, hc1⟩ := execAction_navigate_no_write B D σ1 url
    obtain ⟨hσ2, hc2⟩ := execAction_navigate_no_write B D σ2 url
    rw [hσ1, hσ2, hc1, hc2]
    exact ⟨hlow, chAgree.refl T []⟩
  | networkCall m alias payload pathp target =>
    obtain ⟨hσ1, hc1⟩ := execAction_networkCall_no_write B D σ1 m alias payload pathp target
    obtain ⟨hσ2, hc2⟩ := execAction_networkCall_no_write B D σ2 m alias payload pathp target
    rw [hσ1, hσ2, hc1, hc2]
    exact ⟨hlow, chAgree.refl T []⟩
  | assign t e =>
    by_cases hcs : D.compSyms.contains t = true
    · obtain ⟨hσ1, hc1⟩ := execAction_assign_compSym_no_write B D σ1 t e hcs
      obtain ⟨hσ2, hc2⟩ := execAction_assign_compSym_no_write B D σ2 t e hcs
      rw [hσ1, hσ2, hc1, hc2]
      exact ⟨hlow, chAgree.refl T []⟩
    · have hcs' : D.compSyms.contains t = false := by
        cases h : D.compSyms.contains t
        · rfl
        · exact absurd h hcs
      by_cases htc : T.vars.contains t = true
      · obtain ⟨hw1, hcg1⟩ := execAction_assign_writes_opt B D σ1 t e hcs'
        obtain ⟨hw2, hcg2⟩ := execAction_assign_writes_opt B D σ2 t e hcs'
        exact ⟨lowEq_write_opt hlow htc hw1 hw2, chAgree_write_opt (chAgree.refl T []) htc hcg1 hcg2⟩
      · have hnt : T.vars.contains t = false := by
          cases h : T.vars.contains t
          · rfl
          · exact absurd h htc
        have he : isTaintedE T e = false := stable_assign hst hmem hnt
        have hev := eval_agree B D T hfc (B + 2) σ1 σ2 hlow [] e .init he
        unfold execAction
        split
        · rename_i hc
          rw [hcs'] at hc
          cases hc
        · rw [hev]
          split
          · rename_i v _ _
            exact ⟨lowEq_setVar_same hlow t v, chAgree_cons_same (chAgree.refl T []) t⟩
          · exact ⟨hlow, chAgree.refl T []⟩

/-! ## Effects: navigation-freedom unless the action *is* `navigate`

`Effect.navigate` is emitted **only** by `execAction`'s own `.navigate` arm
(never from expression evaluation — `eval_init_navFree`, and never from the
`.eval`/`.assign`/`.networkCall` arms, which only ever forward expression
effects or append a `.networkCall` marker).  So for any non-`navigate`
action, both runs' contribution to `ungatedNavs` is `[]` *regardless* of
taint — no comparison between the two runs is even needed. -/

theorem execNetPath_navFree (B : Nat) (D : Doc) (σ : Store) (m : Method) (alias : Symbol)
    (pv : Option Val) (pathp : Option Expr) (target : Symbol) (s : EvalSt)
    (h : Inv B s) (hn : navFree s.effects) :
    navFree (execNetPath B D σ m alias pv pathp target s).effects := by
  rw [execNetPath.eq_def]
  split
  · exact navFree_append hn (by intro e he; simp at he; subst he; rfl)
  · rename_i pp
    split
    · rename_i er s' heq
      have o : Out B s ((Except.error er, s') : Res Val) :=
        heq ▸ eval_out B D σ (B + 2) [] pp s h
      obtain ⟨Δ, hΔ, hnΔ⟩ := o.eff
      rw [hΔ]
      exact navFree_append hn hnΔ
    · rename_i v s' heq
      have o : Out B s ((Except.ok v, s') : Res Val) :=
        heq ▸ eval_out B D σ (B + 2) [] pp s h
      obtain ⟨Δ, hΔ, hnΔ⟩ := o.eff
      have hn' : navFree s'.effects := hΔ ▸ navFree_append hn hnΔ
      split
      · rename_i str
        split
        · exact navFree_append hn' (by intro e he; simp at he; subst he; rfl)
        · exact hn'
      · rename_i n
        split
        · exact navFree_append hn' (by intro e he; simp at he; subst he; rfl)
        · exact hn'
      · exact hn'

theorem execAction_networkCall_navFree (B : Nat) (D : Doc) (σ : Store) (m : Method)
    (alias : Symbol) (payload pathp : Option Expr) (target : Symbol) :
    navFree (execAction B D σ (Action.networkCall m alias payload pathp target)).effects := by
  unfold execAction
  split
  · exact execNetPath_navFree B D σ m alias none pathp target EvalSt.init inv_init navFree_nil
  · rename_i p
    split
    · rename_i er s heq
      have o : Out B EvalSt.init ((Except.error er, s) : Res Val) :=
        heq ▸ eval_out B D σ (B + 2) [] p EvalSt.init inv_init
      obtain ⟨Δ, hΔ, hnΔ⟩ := o.eff
      rw [hΔ]
      simpa [EvalSt.init] using navFree_append navFree_nil hnΔ
    · rename_i v s heq
      have o : Out B EvalSt.init ((Except.ok v, s) : Res Val) :=
        heq ▸ eval_out B D σ (B + 2) [] p EvalSt.init inv_init
      refine execNetPath_navFree B D σ m alias (some v) pathp target s o.inv ?_
      obtain ⟨Δ, hΔ, hnΔ⟩ := o.eff
      rw [hΔ]
      simpa [EvalSt.init] using navFree_append navFree_nil hnΔ

/-- Every action's effects are navigation-free, **unless** the action *is*
`navigate` — matching the Rust dispatch asymmetry documented on `Effect` in
`Syntax.lean`.  Phrased as `∀ σ` in the left disjunct because which disjunct
holds is a property of `a` alone (its constructor), not of the store it runs
against — this is exactly the form needed to compare two different stores. -/
theorem execAction_navFree_or_navigate (B : Nat) (D : Doc) (a : Action) :
    (∀ σ, navFree (execAction B D σ a).effects) ∨ ∃ url, a = Action.navigate url := by
  cases a with
  | eval e =>
    left; intro σ
    unfold execAction
    have hnf := eval_init_navFree B D σ (B + 2) [] e
    generalize hE : evalE B D σ (B + 2) [] e EvalSt.init = r at hnf ⊢
    obtain ⟨outcome, s⟩ := r
    cases outcome <;> exact hnf
  | navigate url => right; exact ⟨url, rfl⟩
  | networkCall m alias payload pathp target =>
    left; intro σ
    exact execAction_networkCall_navFree B D σ m alias payload pathp target
  | assign t e =>
    left; intro σ
    unfold execAction
    split
    · exact navFree_nil
    · have hnf := eval_init_navFree B D σ (B + 2) [] e
      generalize hE : evalE B D σ (B + 2) [] e EvalSt.init = r at hnf ⊢
      obtain ⟨outcome, s⟩ := r
      cases outcome <;> exact hnf

/-- `execAction`'s `err = some _` branch never touches the store — true for
every action kind (only a *successful* `assign` ever writes). -/
theorem execAction_err_no_write (B : Nat) (D : Doc) (σ : Store) (a : Action)
    (h : (execAction B D σ a).err ≠ none) : (execAction B D σ a).σ = σ := by
  cases a with
  | eval e => exact (execAction_eval_no_write B D σ e).1
  | navigate url => exact (execAction_navigate_no_write B D σ url).1
  | networkCall m alias payload pathp target =>
    exact (execAction_networkCall_no_write B D σ m alias payload pathp target).1
  | assign t e =>
    revert h
    unfold execAction
    split
    · intro _; rfl
    · split
      · intro hh; exact absurd rfl hh
      · intro _; rfl

/-- `execAction`'s `err = some _` branch never touches `changed` — true for
every action kind, structurally (only a *successful* `assign` ever produces a
non-empty `changed`). -/
theorem execAction_err_changed_nil (B : Nat) (D : Doc) (σ : Store) (a : Action)
    (h : (execAction B D σ a).err ≠ none) : (execAction B D σ a).changed = [] := by
  cases a with
  | eval e => exact (execAction_eval_no_write B D σ e).2
  | navigate url => exact (execAction_navigate_no_write B D σ url).2
  | networkCall m alias payload pathp target =>
    exact (execAction_networkCall_no_write B D σ m alias payload pathp target).2
  | assign t e =>
    revert h
    unfold execAction
    split
    · intro _; rfl
    · split
      · intro hh; exact absurd rfl hh
      · intro _; rfl

/-! ## `chAgree` at the empty list: the "all-tainted" characterisation

`chAgree T ch []` says exactly that `ch` contains no untainted symbol —
the shape `recompute`'s trigger list has whenever it is seeded purely from a
tainted write (a failed action never seeds it at all; a successful *tainted*
`assign` seeds it with exactly that one tainted symbol). -/

theorem chAgree_nil_all_tainted {T : Taint} {ch : List Symbol} (h : chAgree T [] ch) :
    ∀ s ∈ ch, T.vars.contains s = true := by
  intro s hs
  cases hnt : T.vars.contains s
  · exfalso
    have heq := h s hnt
    rw [contains_iff_mem.mpr hs, List.contains_nil] at heq
    exact absurd heq.symm (by decide)
  · rfl

theorem chAgree_all_tainted_nil {T : Taint} {ch : List Symbol}
    (h : ∀ s ∈ ch, T.vars.contains s = true) : chAgree T ch [] := by
  intro s hs
  rw [List.contains_nil]
  cases hc : ch.contains s
  · rfl
  · exact absurd (h s (contains_iff_mem.mp hc)) (by rw [hs]; decide)

/-! ## Recomputation with an empty trigger list is the identity -/

theorem any_contains_nil_false (l : List Symbol) :
    l.any (fun d => ([] : List Symbol).contains d) = false := by
  induction l with
  | nil => rfl
  | cons a rest ih => simp [List.any_cons]

theorem recomputeStep_nil_id (B : Nat) (D : Doc) (acc : RecompRes) (c : CompDef)
    (h : acc.changed = []) :
    (recomputeStep B D acc c).σ = acc.σ ∧ (recomputeStep B D acc c).changed = [] := by
  unfold recomputeStep
  rw [h]
  simp [h]

theorem foldl_recompute_nil_id (B : Nat) (D : Doc) :
    ∀ (l : List CompDef) (acc : RecompRes), acc.changed = [] →
      (l.foldl (recomputeStep B D) acc).σ = acc.σ := by
  intro l
  induction l with
  | nil => intro acc _; rfl
  | cons c rest ih =>
    intro acc h
    obtain ⟨hσ, hch⟩ := recomputeStep_nil_id B D acc c h
    simp only [List.foldl_cons]
    rw [ih (recomputeStep B D acc c) hch, hσ]

/-- With no trigger, `recompute` never fires any comp and leaves the store
untouched — the base case that anchors `recompute_tainted_ch_lowEq_self`. -/
theorem recompute_changed_nil (B : Nat) (D : Doc) (σ : Store) :
    (recompute B D σ []).σ = σ := by
  unfold recompute
  exact foldl_recompute_nil_id B D D.comps ⟨σ, [], 0, [], []⟩ rfl

/-- **Recomputation started from an all-tainted trigger list never disturbs
low-equivalence with its own input.**  Intuitively: an all-tainted `changed`
can never satisfy an untainted comp's (all-untainted, by `collectReads_unt`)
trigger condition, so recomputation can only ever cascade through *other*
tainted comps — every write stays confined to tainted symbols. -/
theorem recompute_tainted_ch_lowEq_self (B : Nat) (D : Doc) (T : Taint) (hfc : FnClosure D T)
    (hst : Taint.stable D T = true) {σ : Store} {ch : List Symbol}
    (h : ∀ s ∈ ch, T.vars.contains s = true) : lowEq T σ (recompute B D σ ch).σ := by
  have hra := (recompute_agree B D T hfc hst (lowEq.refl T σ) (chAgree_all_tainted_nil h)).1
  rw [recompute_changed_nil] at hra
  exact hra.symm

/-! ## Reaction agreement (`fireTx`)

`fireTx` mirrors `execute_and_respond`'s transactional discipline: on
`execAction` failure the store is rolled back and the trace is truncated to
`[]`; on success, `recompute` runs and the (untruncated) effects are tagged
with the reaction's `Ctx`.  The two runs may take *different* branches here
(one run's tainted `assign` may fail while the other's succeeds) — the store
argument below handles that asymmetry explicitly; the trace argument is
handled separately, split on the gate context. -/

/-- The URL of a *non-interactive* `navigate` action is untainted
(`accept_navigate`), so `eval_agree` gives **literal** agreement of the
`err`/`effects` `execAction` produces between the two runs (the `.σ`/
`.changed` fields already agree unconditionally — `execAction_navigate_no_write` —
independently of `url`'s taint, so aren't part of the claim here). -/
theorem execAction_navigate_literal_agree (B : Nat) (D : Doc) (hfc : FnClosure D (taintOf D))
    (hacc : accept D = true) {url : Expr}
    (hmem : (Ctx.nonInteractive, Action.navigate url) ∈ allActions D)
    {σ1 σ2 : Store} (hlow : lowEq (taintOf D) σ1 σ2) :
    (execAction B D σ1 (Action.navigate url)).err = (execAction B D σ2 (Action.navigate url)).err ∧
    (execAction B D σ1 (Action.navigate url)).effects
      = (execAction B D σ2 (Action.navigate url)).effects := by
  have hu : isTaintedE (taintOf D) url = false := accept_navigate hacc hmem
  have hev := eval_agree B D (taintOf D) hfc (B + 2) σ1 σ2 hlow [] url .init hu
  unfold execAction
  rw [hev]
  constructor <;> (split <;> rfl)

/-- If the action's own effects are navigation-free, `fireTx`'s
`nonInteractive`-tagged trace contributes nothing to `ungatedNavs` — the
`recompute` portion never does either (`recompute_navFree`). -/
theorem fireTx_navFree_ungatedNavs_nil (B : Nat) (D : Doc) (σ : Store) (a : Action)
    (hnf : navFree (execAction B D σ a).effects) :
    ungatedNavs (fireTx B D σ Ctx.nonInteractive a).trace = [] := by
  unfold fireTx
  generalize hE : execAction B D σ a = r at hnf ⊢
  obtain ⟨err, σ', effs, w, ch⟩ := r
  cases err with
  | some e => exact ungatedNavs_nil
  | none =>
    simp only at hnf
    rw [ungatedNavs_tag_nonInteractive, navsOfEffects_append,
        navsOfEffects_navFree hnf, navsOfEffects_navFree (recompute_navFree B D σ' ch)]
    rfl

/-- **`fireTx`'s `nonInteractive` trace agrees between the two runs.**  Every
`nonInteractive` action is a root timer (`allActions`); either it is not
`navigate` (both runs contribute `[]` to `ungatedNavs`, independently and
unconditionally), or it *is* `navigate` — the one case the checker actually
gates — and `execAction_navigate_literal_agree` gives literal equality. -/
theorem fireTx_nonInteractive_ungatedNavs_agree (B : Nat) (D : Doc) (hacc : accept D = true)
    {a : Action} (hmem : (Ctx.nonInteractive, a) ∈ allActions D)
    {σ1 σ2 : Store} (hlow : lowEq (taintOf D) σ1 σ2) :
    ungatedNavs (fireTx B D σ1 Ctx.nonInteractive a).trace
      = ungatedNavs (fireTx B D σ2 Ctx.nonInteractive a).trace := by
  rcases execAction_navFree_or_navigate B D a with hnf | ⟨url, hurl⟩
  · rw [fireTx_navFree_ungatedNavs_nil B D σ1 a (hnf σ1),
        fireTx_navFree_ungatedNavs_nil B D σ2 a (hnf σ2)]
  · subst hurl
    have hfc := accept_fnClosure hacc
    obtain ⟨herr, heffs⟩ := execAction_navigate_literal_agree B D hfc hacc hmem hlow
    unfold fireTx
    generalize hE1 : execAction B D σ1 (Action.navigate url) = r1 at herr heffs ⊢
    generalize hE2 : execAction B D σ2 (Action.navigate url) = r2 at herr heffs ⊢
    obtain ⟨err1, σ1', effs1, w1, ch1⟩ := r1
    obtain ⟨err2, σ2', effs2, w2, ch2⟩ := r2
    simp only at herr heffs
    subst herr
    subst heffs
    cases err1 with
    | some e => exact ungatedNavs_nil
    | none =>
      rw [ungatedNavs_tag_nonInteractive, ungatedNavs_tag_nonInteractive,
          navsOfEffects_append, navsOfEffects_append,
          navsOfEffects_navFree (recompute_navFree B D σ1' ch1),
          navsOfEffects_navFree (recompute_navFree B D σ2' ch2)]

/-- Every `gesture` context (`click`/`submit`) contributes nothing to
`ungatedNavs` at all — gate G1 — regardless of what the action does. -/
theorem fireTx_gesture_ungatedNavs_nil (B : Nat) (D : Doc) (σ : Store) (a : Action) :
    ungatedNavs (fireTx B D σ Ctx.gesture a).trace = [] := by
  unfold fireTx
  split
  · exact ungatedNavs_nil
  · exact ungatedNavs_tag_gesture _

/-- **`fireTx` preserves `lowEq` on the store**, for either gate context.
Four cases on the two runs' `execAction` success/failure: same outcome is
closed by `execAction_agree` (+ `recompute_agree` on success); a divergent
outcome is closed by `execAction_err_no_write`/`execAction_err_changed_nil`
identifying the failing side's (unchanged) store with the succeeding side's
*pre-recompute* store, then `recompute_tainted_ch_lowEq_self` — the tainted
`assign` that must be the source of the divergence (`execAction_agree`'s
`chAgree` forces the successful side's `changed` to be all-tainted, since the
failing side's is `[]`) never disturbs low-equivalence. -/
theorem fireTx_lowEq_agree (B : Nat) (D : Doc) (hacc : accept D = true)
    {c : Ctx} {a : Action} (hmem : (c, a) ∈ allActions D)
    {σ1 σ2 : Store} (hlow : lowEq (taintOf D) σ1 σ2) :
    lowEq (taintOf D) (fireTx B D σ1 c a).σ (fireTx B D σ2 c a).σ := by
  have hfc := accept_fnClosure hacc
  have hst := accept_stable hacc
  obtain ⟨hlowA, hchA⟩ := execAction_agree B D (taintOf D) hfc hst hmem hlow
  unfold fireTx
  generalize hE1 : execAction B D σ1 a = r1 at hlowA hchA ⊢
  generalize hE2 : execAction B D σ2 a = r2 at hlowA hchA ⊢
  obtain ⟨err1, σ1', effs1, w1, ch1⟩ := r1
  obtain ⟨err2, σ2', effs2, w2, ch2⟩ := r2
  simp only at hlowA hchA
  cases err1 with
  | some e1 =>
    have h1ne : (execAction B D σ1 a).err ≠ none := by rw [hE1]; simp
    have hσ1eq : σ1' = σ1 := by
      have h := execAction_err_no_write B D σ1 a h1ne; rw [hE1] at h; simpa using h
    have hch1eq : ch1 = [] := by
      have h := execAction_err_changed_nil B D σ1 a h1ne; rw [hE1] at h; simpa using h
    cases err2 with
    | some e2 => exact hlow
    | none =>
      have htn : ∀ s ∈ ch2, (taintOf D).vars.contains s = true := by
        rw [hch1eq] at hchA; exact chAgree_nil_all_tainted hchA
      have hself := recompute_tainted_ch_lowEq_self B D (taintOf D) hfc hst (σ := σ2') (ch := ch2) htn
      rw [hσ1eq] at hlowA
      exact hlowA.trans hself
  | none =>
    cases err2 with
    | some e2 =>
      have h2ne : (execAction B D σ2 a).err ≠ none := by rw [hE2]; simp
      have hσ2eq : σ2' = σ2 := by
        have h := execAction_err_no_write B D σ2 a h2ne; rw [hE2] at h; simpa using h
      have hch2eq : ch2 = [] := by
        have h := execAction_err_changed_nil B D σ2 a h2ne; rw [hE2] at h; simpa using h
      have htn : ∀ s ∈ ch1, (taintOf D).vars.contains s = true := by
        rw [hch2eq] at hchA; exact chAgree_nil_all_tainted hchA.symm
      have hself := recompute_tainted_ch_lowEq_self B D (taintOf D) hfc hst (σ := σ1') (ch := ch1) htn
      rw [hσ2eq] at hlowA
      exact (hself.symm.trans hlowA)
    | none => exact (recompute_agree B D (taintOf D) hfc hst hlowA hchA).1

/-- **`fireTx` agreement**, combining the store and trace halves. -/
theorem fireTx_agree (B : Nat) (D : Doc) (hacc : accept D = true)
    {c : Ctx} {a : Action} (hmem : (c, a) ∈ allActions D)
    {σ1 σ2 : Store} (hlow : lowEq (taintOf D) σ1 σ2) :
    lowEq (taintOf D) (fireTx B D σ1 c a).σ (fireTx B D σ2 c a).σ ∧
    ungatedNavs (fireTx B D σ1 c a).trace = ungatedNavs (fireTx B D σ2 c a).trace := by
  refine ⟨fireTx_lowEq_agree B D hacc hmem hlow, ?_⟩
  cases c with
  | gesture =>
    rw [fireTx_gesture_ungatedNavs_nil B D σ1 a, fireTx_gesture_ungatedNavs_nil B D σ2 a]
  | nonInteractive => exact fireTx_nonInteractive_ungatedNavs_agree B D hacc hmem hlow

/-! ## Reaction agreement (`fireSubmit`) -/

/-- `fireSubmit` always emits a `gesture`-tagged trace (both the "action
failed" and "action succeeded" branches), so it never contributes to
`ungatedNavs` — no comparison between the two runs is needed here either. -/
theorem fireSubmit_ungatedNavs_nil (B : Nat) (D : Doc) (σ0 : Store) (formSym : Symbol)
    (a : Action) : ungatedNavs (fireSubmit B D σ0 formSym a).trace = [] := by
  unfold fireSubmit
  split <;> exact ungatedNavs_tag_gesture _

/-- **`fireSubmit` preserves `lowEq`.**  Unlike `fireTx`, *both* branches of
`fireSubmit` call `recompute` (the `$form` write survives action failure), so
the asymmetric case needs no special "tainted self" lemma: `execAction_agree`
already gives `chAgree` between the two `changed` outputs (`[]` vs. the
failing side's own `[]` after substitution), and prepending the same
`formSym` (`chAgree_cons_same`) keeps them in lockstep for `recompute_agree`
directly. -/
theorem fireSubmit_lowEq_agree (B : Nat) (D : Doc) (hacc : accept D = true)
    {a : Action} (hmem : (Ctx.gesture, a) ∈ allActions D) (formSym : Symbol)
    {σ0_1 σ0_2 : Store} (hlow : lowEq (taintOf D) σ0_1 σ0_2) :
    lowEq (taintOf D) (fireSubmit B D σ0_1 formSym a).σ (fireSubmit B D σ0_2 formSym a).σ := by
  have hfc := accept_fnClosure hacc
  have hst := accept_stable hacc
  obtain ⟨hlowA, hchA⟩ := execAction_agree B D (taintOf D) hfc hst hmem hlow
  unfold fireSubmit
  generalize hE1 : execAction B D σ0_1 a = r1 at hlowA hchA ⊢
  generalize hE2 : execAction B D σ0_2 a = r2 at hlowA hchA ⊢
  obtain ⟨err1, σ1', effs1, w1, ch1⟩ := r1
  obtain ⟨err2, σ2', effs2, w2, ch2⟩ := r2
  simp only at hlowA hchA
  cases err1 with
  | some e1 =>
    have h1ne : (execAction B D σ0_1 a).err ≠ none := by rw [hE1]; simp
    have hch1eq : ch1 = [] := by
      have h := execAction_err_changed_nil B D σ0_1 a h1ne; rw [hE1] at h; simpa using h
    cases err2 with
    | some e2 =>
      exact (recompute_agree B D (taintOf D) hfc hst hlow (chAgree.refl (taintOf D) [formSym])).1
    | none =>
      have hσ1eq : σ1' = σ0_1 := by
        have h := execAction_err_no_write B D σ0_1 a h1ne; rw [hE1] at h; simpa using h
      rw [hσ1eq] at hlowA
      have hch2 : chAgree (taintOf D) [formSym] (formSym :: ch2) := by
        rw [hch1eq] at hchA; exact chAgree_cons_same hchA formSym
      exact (recompute_agree B D (taintOf D) hfc hst hlowA hch2).1
  | none =>
    cases err2 with
    | some e2 =>
      have h2ne : (execAction B D σ0_2 a).err ≠ none := by rw [hE2]; simp
      have hch2eq : ch2 = [] := by
        have h := execAction_err_changed_nil B D σ0_2 a h2ne; rw [hE2] at h; simpa using h
      have hσ2eq : σ2' = σ0_2 := by
        have h := execAction_err_no_write B D σ0_2 a h2ne; rw [hE2] at h; simpa using h
      rw [hσ2eq] at hlowA
      have hch1 : chAgree (taintOf D) (formSym :: ch1) [formSym] := by
        rw [hch2eq] at hchA; exact chAgree_cons_same hchA formSym
      exact (recompute_agree B D (taintOf D) hfc hst hlowA hch1).1
    | none =>
      exact (recompute_agree B D (taintOf D) hfc hst hlowA (chAgree_cons_same hchA formSym)).1

/-- **`fireSubmit` agreement**, combining the store and (trivial) trace
halves. -/
theorem fireSubmit_agree (B : Nat) (D : Doc) (hacc : accept D = true)
    {a : Action} (hmem : (Ctx.gesture, a) ∈ allActions D) (formSym : Symbol)
    {σ0_1 σ0_2 : Store} (hlow : lowEq (taintOf D) σ0_1 σ0_2) :
    lowEq (taintOf D) (fireSubmit B D σ0_1 formSym a).σ (fireSubmit B D σ0_2 formSym a).σ ∧
    ungatedNavs (fireSubmit B D σ0_1 formSym a).trace
      = ungatedNavs (fireSubmit B D σ0_2 formSym a).trace := by
  refine ⟨fireSubmit_lowEq_agree B D hacc hmem formSym hlow, ?_⟩
  rw [fireSubmit_ungatedNavs_nil B D σ0_1 formSym a, fireSubmit_ungatedNavs_nil B D σ0_2 formSym a]

/-! ## Reaction and run agreement — T2 -/

theorem click_mem_allActions {D : Doc} {i : Nat} {a : Action} (h : D.clicks[i]? = some a) :
    (Ctx.gesture, a) ∈ allActions D := by
  have hmem : a ∈ D.clicks := List.mem_of_getElem? h
  unfold allActions
  exact List.mem_append_left _ (List.mem_append_left _ (List.mem_map_of_mem hmem))

theorem timer_mem_allActions {D : Doc} {i : Nat} {a : Action} (h : D.timers[i]? = some a) :
    (Ctx.nonInteractive, a) ∈ allActions D := by
  have hmem : a ∈ D.timers := List.mem_of_getElem? h
  unfold allActions
  exact List.mem_append_right _ (List.mem_map_of_mem hmem)

theorem submit_mem_allActions {D : Doc} {i : Nat} {a : Action} (h : D.submits[i]? = some a) :
    (Ctx.gesture, a) ∈ allActions D := by
  have hmem : a ∈ D.submits := List.mem_of_getElem? h
  unfold allActions
  exact List.mem_append_left _ (List.mem_append_right _ (List.mem_map_of_mem hmem))

/-- Two events are "the same reaction, differing only in untrusted payload":
same click/timer index, same submit index (only the submitted `fields` may
differ), same `netResponse` target symbol *that is a genuine network-call
target* (only the delivered value may differ — an arbitrary undeclared
symbol is not a legitimate `netResponse`, and is out of scope, matching
`netTarget_tainted`'s own domain). -/
inductive EventAgree (D : Doc) : Event → Event → Prop
  | click (i : Nat) : EventAgree D (.click i) (.click i)
  | timer (i : Nat) : EventAgree D (.timer i) (.timer i)
  | submit (i : Nat) (f1 f2 : List (String × Val)) :
      EventAgree D (.submit i f1) (.submit i f2)
  | netResponse (t : Symbol) (ht : t ∈ netTargets D) (v1 v2 : Val) :
      EventAgree D (.netResponse t v1) (.netResponse t v2)

/-- Pointwise lifting of `EventAgree` to event sequences: same length, same
shape at every position. -/
inductive EventsAgree (D : Doc) : List Event → List Event → Prop
  | nil : EventsAgree D [] []
  | cons {e1 e2 : Event} {es1 es2 : List Event} :
      EventAgree D e1 e2 → EventsAgree D es1 es2 → EventsAgree D (e1 :: es1) (e2 :: es2)

/-- **Reaction agreement**: one input event, agreeing runs in, agreeing runs
out — composing `fireTx_agree`/`fireSubmit_agree`/`recompute_agree` per
`Event` constructor. -/
theorem reaction_agree (B : Nat) (D : Doc) (hacc : accept D = true)
    {e1 e2 : Event} (he : EventAgree D e1 e2)
    {σ1 σ2 : Store} (hlow : lowEq (taintOf D) σ1 σ2) :
    lowEq (taintOf D) (reaction B D σ1 e1).σ (reaction B D σ2 e2).σ ∧
    ungatedNavs (reaction B D σ1 e1).trace = ungatedNavs (reaction B D σ2 e2).trace := by
  have hfc := accept_fnClosure hacc
  have hst := accept_stable hacc
  cases he with
  | click i =>
    simp only [reaction]
    split
    · exact ⟨hlow, rfl⟩
    · rename_i a heq
      exact fireTx_agree B D hacc (click_mem_allActions heq) hlow
  | timer i =>
    simp only [reaction]
    split
    · exact ⟨hlow, rfl⟩
    · rename_i a heq
      exact fireTx_agree B D hacc (timer_mem_allActions heq) hlow
  | submit i f1 f2 =>
    simp only [reaction]
    have hff : lowEq (taintOf D) (setVar σ1 D.formSym (Val.record f1))
        (setVar σ2 D.formSym (Val.record f2)) :=
      lowEq_write_opt hlow (formSym_tainted D) (Or.inr ⟨_, rfl⟩) (Or.inr ⟨_, rfl⟩)
    split
    · have hr := recompute_agree B D (taintOf D) hfc hst hff (chAgree.refl (taintOf D) [D.formSym])
      refine ⟨hr.1, ?_⟩
      rw [ungatedNavs_tag_gesture, ungatedNavs_tag_gesture]
    · rename_i a heq
      exact fireSubmit_agree B D hacc (submit_mem_allActions heq) D.formSym hff
  | netResponse t ht v1 v2 =>
    simp only [reaction]
    have htt : (taintOf D).vars.contains t = true := netTarget_tainted ht
    split
    · have hσ : lowEq (taintOf D) (setVar σ1 t v1) (setVar σ2 t v2) :=
        lowEq_write_opt hlow htt (Or.inr ⟨_, rfl⟩) (Or.inr ⟨_, rfl⟩)
      have hr := recompute_agree B D (taintOf D) hfc hst hσ (chAgree.refl (taintOf D) [t])
      refine ⟨hr.1, ?_⟩
      rw [ungatedNavs_tag_nonInteractive, ungatedNavs_tag_nonInteractive,
          navsOfEffects_navFree (recompute_navFree B D (setVar σ1 t v1) [t]),
          navsOfEffects_navFree (recompute_navFree B D (setVar σ2 t v2) [t])]
    · exact ⟨hlow, rfl⟩

/-- **Run agreement**: folding `reaction_agree` over the whole event
sequence, combining the store half by transitivity and the trace half via
`ungatedNavs_append`. -/
theorem run_agree (B : Nat) (D : Doc) (hacc : accept D = true) :
    ∀ {evs1 evs2 : List Event}, EventsAgree D evs1 evs2 →
      ∀ {σ1 σ2 : Store}, lowEq (taintOf D) σ1 σ2 →
        lowEq (taintOf D) (run B D σ1 evs1).1 (run B D σ2 evs2).1 ∧
        ungatedNavs (run B D σ1 evs1).2 = ungatedNavs (run B D σ2 evs2).2 := by
  intro evs1 evs2 hevs
  induction hevs with
  | nil =>
    intro σ1 σ2 hlow
    exact ⟨hlow, rfl⟩
  | cons he _ ih =>
    intro σ1 σ2 hlow
    obtain ⟨hlowR, hnavR⟩ := reaction_agree B D hacc he hlow
    obtain ⟨hlowRest, hnavRest⟩ := ih hlowR
    simp only [run]
    refine ⟨hlowRest, ?_⟩
    rw [ungatedNavs_append, ungatedNavs_append, hnavR, hnavRest]

/-- **T2 — Non-interference (the composed theorem).**

For a document accepted by the flow checker (`accept D = true`): any two runs
that start from low-equivalent stores (`lowEq (taintOf D) σ1 σ2`) and process
event sequences that agree event-by-event (`EventsAgree D evs1 evs2` — same
click/timer index at each position, same submit index, same `netResponse`
target symbol, but the **submitted field values and the delivered
`netResponse` values may differ arbitrarily** between the two runs) end in
low-equivalent stores, and — the actual security payoff — emit the *exact
same sequence* of ungated (non-gesture) navigation requests.

**Scope, precisely — read together with `eval_agree`'s and `NonInterference.lean`'s
header comment, which this theorem composes end to end:**

* *"Low-equivalent"* (`lowEq`) means agreement on every symbol the checker
  did **not** mark tainted.  It says nothing about the values of *tainted*
  variables, and nothing about `$form` itself (a taint source by
  construction) — those may differ freely between the two runs.
* *"Ungated navigation requests"* (`ungatedNavs`) is exactly the
  `.navigate` effects emitted from a `nonInteractive` context (root timers).
  Gesture-triggered navigation (click/submit) is discharged by gate G1 and is
  *not* part of the claim — an attacker-influenced click handler can still
  navigate anywhere it likes; T2 is a statement about the *involuntary* path.
* `storeLocal`/`copyClipboard`/`download`/`getSystemTime` effects, and the
  full content (not just the alias) of `networkCall` effects, are **not**
  claimed to agree between the two runs.  This is the same, already-documented
  scope limit `eval_agree` carries (see the file header): a tainted
  `assign`/`comp` may freely diverge in what it writes to *tainted* storage
  keys, clipboard targets, or download aliases, and in the payload of a
  network call whose `path_param`/body was computed from tainted data.  T2
  makes no promise there — that is accepted residual scope, not a gap in
  this proof.  `getSystemTime`'s *target Symbol* is the one part of it that
  **is** covered — fixed at parse time (RM-04, `parser::logic.rs`) and
  load-time-checked against `comp` collisions (`parser::flow.rs`), exactly
  like `Assign`'s target; only the delivered timestamp value itself (never
  attacker-influenced) is out of scope, same as the other three.
* The composition chain, bottom to top, is: `eval_agree` (single expression)
  → `recomputeStep_agree`/`execAction_agree` (one comp / one action) →
  `fireTx_agree`/`fireSubmit_agree` (one full reaction's transactional
  discipline) → `reaction_agree` (one input event, all four kinds) →
  `run_agree` (a whole event trace) → this theorem (restating `run_agree`
  under the name the security invariant is known by). Every step is a
  genuine Lean proof term; there is no `sorry`/`axiom` anywhere in the
  chain. -/
theorem T2_non_interference (B : Nat) (D : Doc) (hacc : accept D = true)
    {evs1 evs2 : List Event} (hevs : EventsAgree D evs1 evs2)
    {σ1 σ2 : Store} (hlow : lowEq (taintOf D) σ1 σ2) :
    lowEq (taintOf D) (run B D σ1 evs1).1 (run B D σ2 evs2).1 ∧
    ungatedNavs (run B D σ1 evs1).2 = ungatedNavs (run B D σ2 evs2).2 :=
  run_agree B D hacc hevs hlow

end Mizu
