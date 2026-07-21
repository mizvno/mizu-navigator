# Design memo — Responsive layout (ux-6)

**Status: APPROVED (signed off).** Option (a) approved as written; `vh`
excludes `CHROME_HEIGHT`; scope confirmed as window-width only (no
container queries). Phase 2 implements exactly this.

This memo is Phase 1 of ux-6 (two-phase per the prompt: design, then implement only
after approval). It decides *what* the syntax is, not how it's coded. Phase 2 will
cite this file's decisions as its spec.

## Verified starting state

- `parser/style.rs::parse_dimension` accepts only a bare `f32` (pixels) or `f32%`
  (percent of parent). No `vw`/`vh`/`vmin`/`vmax`. No `@media`, `viewport`, or
  `breakpoint` token anywhere in the grammar (grep-confirmed).
- The window is resized via `WindowEvent::Resized`
  (`src/render/window/event_loop.rs`), already debounced: a resize only triggers
  `MizuWindowManager::resize_viewport` once per ≥16ms
  (`manager.last_layout_time` / `pending_resize`), the remainder deferred to the
  next tick. **Phase 2 needs no new debounce mechanism** — breakpoint
  re-evaluation rides the existing one, since it must run at the same point
  `resize_viewport` already does (new window size → new Taffy tree).
- `StyleRules::merge` (`parser/style.rs`) is the existing "later rules win,
  unset fields don't clobber" overlay: `tag rules → class rules`, called from
  both `text_engine.rs` and `vello_pipeline.rs`. This is the mechanism every
  option below is measured against reusing.
- A **different, existing** mechanism must not be confused with what's being
  proposed here: `MizuNode.conditional_classes` (`class X if <expr>` in the
  layout block) applies an *extra class* to one node, gated by a **document
  logic expression**, re-evaluated by the `StateMachine` every frame. What
  this memo proposes is gated by **environment state** (window width, OS
  theme) with no document-logic expression involved at all. They are
  complementary axes — logic-driven per-node extras vs. environment-driven
  rule-set variants — not competing designs, and Phase 2 must not merge them
  into one system; that would let environment state leak into the expression
  evaluator's surface (a Phase-1-adjacent invariant concern, see the
  Security note below).
- Node budget: `MAX_SYNTHETIC_LAYOUT_NODES` (`render/layout_bridge.rs`,
  invariant L1) bounds `each`-loop expansion. Nothing proposed here touches
  `each` or introduces per-breakpoint subtree duplication — confirmed in the
  design itself (see "Why L1 is untouched" below), and Phase 2's test suite
  must still assert it.

## 1. Viewport units — decided: yes, add `vw` / `vh` / `vmin` / `vmax`

Low-risk, additive, and the natural conjugate to pixels/percent that
`parse_dimension` already distinguishes by suffix. Decision:

- `vw` = 1% of the **document content viewport width** (the window's logical
  width — the chrome bar has no horizontal inset, so this is just window
  width).
- `vh` = 1% of the **document content viewport height**, i.e. window logical
  height **minus `CHROME_HEIGHT`** (the chrome bar is not part of the
  document's viewport — a document's `100vh` should fill exactly the space
  it can paint into, not include pixels the chrome bar already owns).
- `vmin` = `min(vw, vh)`, `vmax` = `max(vw, vh)`, computed from the values
  above (not independently re-derived).
- Resolved at the same point pixels/percent already are — inside the layout
  pass, from the window size already known before layout runs. This is a
  pure function of an *input* (window size), never a layout *output*, so it
  introduces no re-entrancy (see the rejected option 2c below for why that
  distinction matters).

Grammar: `dimension_value = number | number "%" | number "vw" | number "vh" | number "vmin" | number "vmax"`.
Same bare-number-plus-suffix shape the codebase already uses for `%` — no new
tokenization strategy needed.

## 2. Breakpoints — decided: option (a), window-width variants on the selector line

### Options considered

**(a) Window-width variants in the style block, reusing `StyleRules::merge`.**
A rule set for a selector gains an optional trailing condition; the condition
is evaluated once per layout pass against the current window width; matching
variants merge over the base rules exactly like today's tag→class overlay.
No new block type, no new indentation level, no second merge algorithm.

**(b) A CSS `@media`-style wrapping block.** Rejected. This is a second block
*type* (a new indentation scope with its own selectors nested inside),
doubling the surface `parse_style` has to understand for a capability (a" the
same rule set, applied conditionally") that (a) gets with one optional suffix
on the selector line already being tokenized. More familiar to web authors,
but "familiar to web authors" is not this project's tie-breaker — MANIFESTO's
"reads top to bottom" and "small predictable surface" are, and (a) is smaller
by construction: it's a filter on the existing merge order, not a new grammar
production.

**(c) A layout-level conditional (`each`-like node).** Rejected. This would
make responsiveness a *structural* (DOM-shape) decision instead of a *style*
(paint-time) decision, which creates exactly the two problems the security
guardrail calls out:
1. **Node budget risk.** An `each`-like conditional construct sits in the
   layout tree, which is the thing `MAX_SYNTHETIC_LAYOUT_NODES` bounds. A
   structural conditional invites (even if not required to) multiple
   subtrees existing simultaneously (one per breakpoint arm) prior to
   selection, which is a fundamentally different — and harder to keep
   O(1)-per-node — budget shape than "one subtree, some of whose *paint*
   properties vary."
2. **Re-entrancy.** Layout structure normally depends only on the DOM +
   styles, resolved in one pass. A layout-level conditional keyed on
   *window width* would need window width available *during* layout-tree
   construction, and (per width variants that could change a node's
   presence/children) potentially need to re-run tree construction when
   width crosses a threshold mid-frame — a feedback loop layout doesn't
   otherwise have. Style-level variants avoid this entirely: window width is
   read once, before layout, as a plain input — never a layout output feeding
   back into the same pass.

### Decided syntax

```ebnf
selector_line     = selector { SP+ variant_condition } ;
variant_condition = "@min-width" SP+ integer
                   | "@max-width" SP+ integer
                   | "@dark"
                   | "@light" ;
```

Example:

```
.sidebar
    width 240
    direction column

.sidebar @max-width 599
    width 100%
    direction row
```

- Multiple conditions on one selector line combine with **AND**
  (`.card @min-width 600 @max-width 900` — a range).
- Thresholds are bare integers in logical pixels (`@min-width 600`, not
  `@min-width 600px`) — consistent with `width`/`padding`/etc. never taking a
  unit suffix for pixels today.
- **`@dark` / `@light` are included in this same grammar production**, not a
  parallel one. ux-5 shipped the chrome-side theming but explicitly deferred
  the document-side `@dark`/`@light` style variant as a follow-up (see that
  commit's module doc in `render/preferences.rs`) precisely so it could be
  designed together with ux-6 instead of inventing a second selector-suffix
  mechanism. This memo is that follow-up's design: `@dark`/`@light` and
  `@min-width`/`@max-width` are both `variant_condition`s, resolved by the
  same function against the same kind of environment snapshot.

### Resolution semantics

A rule set whose selector carries one or more `variant_condition`s is only
merged in when **all** of its conditions currently hold, evaluated against a
per-layout-pass `RenderEnvironment { window_width: f32, color_scheme:
ColorScheme }` snapshot (color_scheme already exists as
`preferences::UserPreferences::color_scheme`, ux-5). Merge order:

```
effective = tag_rules
              .merge(unconditioned_class_rules)
              .merge(matching_conditioned_rules_in_declaration_order)
```

— i.e. exactly today's `tag → class` overlay, with one more overlay pass
appended: conditioned variants for the same selector, filtered by
`RenderEnvironment`, applied in the order they appear in the source (later
wins ties, matching every other "declaration order" rule already in the
grammar). This is why the guardrail's "evaluation is O(1) per node per frame"
holds: resolving a node's effective style is still "look up its tag/class
key(s) in the map," now filtered by a cheap condition check per candidate
rule set — no window-width-dependent tree walk, no per-node recomputation
proportional to document size beyond what merging already costs today.

### Why L1 is untouched

Breakpoints only change which `StyleRules` a selector resolves to. They never
touch `parse_layout`'s node construction, `each` expansion, or
`layout_bridge::expand_each_nodes`'s budget counter. A document with N
breakpoint variants across its stylesheet still produces exactly the DOM node
count `parse_layout` always produced — variants are alternatives *within* one
`HashMap<String, StyleRules>` entry's resolution, not additional nodes. Phase
2's test suite asserts this directly (build the same document at a width
below and above a breakpoint, assert `dom.nodes().count()` — or the
equivalent synthetic/Taffy node count — is identical).

## 3. Naming collision — flagged for ux-7

`direction` (existing) is the flex axis (`row` | `column`) —
`taffy::style::FlexDirection`. CSS's `direction: rtl | ltr` is a *different*
concept (text/writing direction), and ux-7 (bidi/i18n) will need to name
that concept. **They must not collide.** This memo does not decide ux-7's
name (that's ux-7's own two-phase design gate to run), but flags the
constraint explicitly so both memos can be checked against each other before
either is implemented: candidate non-colliding names for ux-7 to consider are
`text-direction` or `writing-direction`; whichever ux-7 picks, if it also
wants an environment-gated variant (e.g. `@rtl`/`@ltr`), it should be a third
`variant_condition` arm in the *same* grammar production this memo defines,
not a fourth parallel mechanism.

## Security posture (unchanged, restated for this memo)

Responsiveness is pure render-time layout/style math: no capability, no I/O,
no taint, no logic-callable primitive exposing window size or the resolved
variant (mirrors ux-5's S1/F1 posture for OS preferences — window width is an
*input to paint*, never a value a document expression can read). L1 (node
budget) is unaffected per the analysis above. No re-entrancy is introduced:
window width is read once per layout pass as a plain input.

## Cross-references

- ux-5 (`render/preferences.rs`): `color_scheme` is the value one
  `variant_condition` arm (`@dark`/`@light`) reads; this memo is the deferred
  design for consuming it on the document side.
- ux-7 (bidi/i18n, not yet started): must pick a non-colliding name for
  text/writing direction and, if it wants an environment-gated variant,
  reuse this memo's `variant_condition` grammar rather than forking a third.

## Decisions confirmed at sign-off

1. **Threshold syntax:** `@min-width` / `@max-width` with bare pixel
   integers, AND-combined — the direct numeric form, no named-breakpoint
   indirection layer.
2. **`vh` semantics:** excludes `CHROME_HEIGHT` — a document's `100vh` fills
   exactly the paintable document area, not the full window.
3. **Scope:** window-width only. Container queries (per-node/per-container
   width conditions) are explicitly out of scope — the re-entrancy argument
   in rejected option 2c applies equally to a per-container width, which is
   itself a layout output.

---

**Phase 1 complete and signed off.** Phase 2 (implementation) proceeds in a
separate commit, implementing exactly the decisions above.
