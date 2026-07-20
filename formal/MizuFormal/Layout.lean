import MizuFormal.Semantics

/-!
# λ_mizu — Layout expansion under the node budget

Mirrors `render::layout_bridge::expand_each_nodes`
(`layout_bridge.rs::expand_each_nodes`) at the budget-relevant level: per
`each` node, the number of expanded rows is
clamped to `remaining_budget / (template_size + 1)`, and the created synthetic
nodes are deducted from the shared budget (invariant L1).

The visual content of the expansion is irrelevant to the theorems; what is
modeled is exactly the *counting*: how many synthetic layout nodes one frame
may create, as a function of the store.
-/

namespace Mizu

/-- Rows requested by an `each` node: the length of the bound list variable,
`0` when the variable is absent or not a list (the per-`each_dom_id` loop in
`layout_bridge.rs::expand_each_nodes`). -/
def eachRows (σ : Store) (sp : EachSpec) : Nat :=
  match alookup σ sp.listVar with
  | some (.list xs) => xs.length
  | _ => 0

/-- One `each` node: `(created, remaining) → (created', remaining')`
(the budget-clamping logic in `layout_bridge.rs::expand_each_nodes`). -/
def expandStep (σ : Store) (acc : Nat × Nat) (sp : EachSpec) : Nat × Nat :=
  let (created, remaining) := acc
  let n := eachRows σ sp
  if n == 0 then acc
  else
    let perRow := sp.templateSize + 1
    let maxRows := remaining / perRow
    let c := min n maxRows
    (created + c * perRow, remaining - c * perRow)

/-- Total synthetic layout nodes created by one frame's expansion, starting
from the full budget `N` (`MAX_SYNTHETIC_LAYOUT_NODES`). -/
def expandLayout (N : Nat) (D : Doc) (σ : Store) : Nat :=
  (D.eachSpecs.foldl (expandStep σ) (0, N)).1

/-- The conserved quantity: a step never increases `created + remaining`. -/
theorem expandStep_sum_le (σ : Store) (acc : Nat × Nat) (sp : EachSpec) :
    (expandStep σ acc sp).1 + (expandStep σ acc sp).2 ≤ acc.1 + acc.2 := by
  obtain ⟨created, remaining⟩ := acc
  simp only [expandStep]
  split
  · exact Nat.le_refl _
  · have hle : min (eachRows σ sp) (remaining / (sp.templateSize + 1)) * (sp.templateSize + 1)
        ≤ remaining := by
      calc min (eachRows σ sp) (remaining / (sp.templateSize + 1)) * (sp.templateSize + 1)
          ≤ (remaining / (sp.templateSize + 1)) * (sp.templateSize + 1) :=
            Nat.mul_le_mul_right _ (Nat.min_le_right _ _)
        _ ≤ remaining := Nat.div_mul_le_self _ _
    omega

theorem foldl_expand_sum_le (σ : Store) :
    ∀ (l : List EachSpec) (acc : Nat × Nat),
      (l.foldl (expandStep σ) acc).1 + (l.foldl (expandStep σ) acc).2 ≤ acc.1 + acc.2 := by
  intro l
  induction l with
  | nil => intro acc; exact Nat.le_refl _
  | cons sp rest ih =>
    intro acc
    calc (List.foldl (expandStep σ) (expandStep σ acc sp) rest).1
          + (List.foldl (expandStep σ) (expandStep σ acc sp) rest).2
        ≤ (expandStep σ acc sp).1 + (expandStep σ acc sp).2 := ih _
      _ ≤ acc.1 + acc.2 := expandStep_sum_le σ acc sp

/-- **L1 (layout budget)**: no store — hence no remote data of any size —
makes one frame create more than `N` synthetic layout nodes. -/
theorem expandLayout_le (N : Nat) (D : Doc) (σ : Store) :
    expandLayout N D σ ≤ N := by
  have h := foldl_expand_sum_le σ D.eachSpecs (0, N)
  simp only [expandLayout]
  omega

end Mizu
