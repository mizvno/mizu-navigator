# Design memo — Bidirectional text & logical properties (ux-7)

**Status: APPROVED (signed off), with one scope expansion.** §3's
`direction` → `flex-direction` rename approved as written. §4's two-tier
control-character policy approved as written. §2's scope was **expanded at
sign-off**: flex-container mirroring (reversing a `row` container's child
order under resolved RTL) is now **in scope** for Phase 2, not deferred —
see the revised §2 below. Physical per-side properties and block-axis
logical properties remain out of scope (not requested at sign-off).

This memo is Phase 1 of ux-7 (two-phase per the prompt: design, then implement
only after approval). It decides *what* the syntax and policy are, not how
they're coded. Phase 2 will cite this file's decisions as its spec.

## Verified starting state (corrections to the prompt's own claims)

- **The font-fallback claim is stale.** The prompt states the fallback chain
  "deliberately includes Meiryo / Yu Gothic / Hiragino Sans". That was true
  before ux-3; `render/text_engine.rs` now resolves the author's
  `font-family` (`sans-serif`/`serif`/`monospace`) to a single
  `parley::GenericFamily` entry and lets fontique's own script-aware
  fallback do the rest (see that module's doc). No hardcoded per-script
  named list exists anymore. This doesn't change ux-7's problem (there is
  still no base-direction or logical-property story), but the "why CJK
  already half-works" framing needs updating.
- **Parley already does full bidi reordering internally — verified by
  reading the source, not assumed.** `parley::bidi::BidiResolver` (backed by
  `icu_properties::props::BidiClass`) implements the complete Unicode Bidi
  Algorithm and is invoked automatically from `analysis/mod.rs` for every
  layout that contains bidi-relevant characters. **There is nothing for
  Phase 2 to implement here** — intra-paragraph reordering of a mixed
  LTR/RTL line is already correct today, for any text passed to
  `calculate_node_text`/`build_chrome_text_layout`, with zero code changes.
  This significantly shrinks Phase 2's scope relative to what the prompt
  assumes.
- **Base direction has no public override in parley 0.10 — verified against
  the actual call site.** `BidiResolver::resolve(chars, base_level:
  Option<u8>)` accepts an explicit base level, but the internal caller in
  `analysis/mod.rs` always passes `None`, which falls back to
  `default_level()` — the standard UAX#9 P2/P3 "first strong character"
  auto-detection. Parley's public `RangedBuilder` API exposes no parameter
  to force a different base level. The only lever available without
  forking parley: **prepend a zero-width strong-directional mark** — U+200F
  RLM to force RTL, U+200E LRM to force LTR — to the text handed to the
  builder. This is a standard, well-precedented technique (the auto-detect
  algorithm scans the actual characters, including any we insert), not a
  workaround of last resort.
- **Mizu has no physical per-side margin/padding today, so there's nothing
  to "keep working."** `StyleRules::margin`/`padding` are single, uniform
  `MizuDimension` values applied to all four sides (`parser/style.rs`) —
  there is no `margin-left`/`margin-top`/etc. at all yet, physical or
  logical. Introducing `margin-inline-start`/`-end` is new per-side surface
  from scratch, not a logical variant of an existing physical one. This
  memo scopes that surface deliberately narrow (see §2).
- **`LAYOUT_ATTR_KEYWORDS`** (`parser/layout.rs:172`) is
  `["class", "id", "src", "href", "alt"]` — confirmed current, `dir` is not
  in it.
- **The URL-bar input filter is confirmed insufficient for the guardrail's
  own threat.** `chrome_vello.rs:402` filters typed text with
  `.filter(|c| !c.is_control())`. Unicode bidi override/embedding
  characters (U+202A–U+202E) and isolates (U+2066–U+2069) are category
  **Cf (Format)**, not **Cc (Control)** — `char::is_control()` does not
  catch them. They pass through today, unfiltered, into the URL bar. This
  is the concrete gap §4 closes.

## 1. Base direction — decided: `dir` layout attribute, default `auto`

`dir="ltr" | "rtl" | "auto"` on a layout node (`window`, `box`, `text`,
`button`, `input`, `image`, `markdown`), added to `LAYOUT_ATTR_KEYWORDS`.
Inherits down the tree like HTML's `dir` (a node without an explicit `dir`
uses its nearest ancestor's resolved value; the root defaults to `auto`).

**Why an attribute, not a style property, and why `auto` by default:**

- `dir` is content metadata (what script/language is this subtree in), not a
  visual preference — the same distinction Mizu already draws elsewhere
  (`class`/`id`/`href` are layout attributes; visual choices go in the style
  block). Putting it in the style block would blur that line for no benefit,
  since direction isn't something a class overlay or breakpoint needs to
  override independently of the content it describes (see §2 for why this
  also means no `@rtl`/`@ltr` ux-6 variant condition).
- **Against Mizu's "explicit over implicit" leaning:** auto-detect sounds
  like the implicit choice, but it is the **already-happening default**
  (parley auto-detects internally regardless of what we do — see above), so
  making `auto` the attribute default doesn't introduce new implicit
  behavior; it just gives an author an explicit escape hatch (`dir="rtl"`)
  for the cases auto-detection gets wrong (a short Arabic phrase starting
  with a Latin brand name, a number, or punctuation — exactly where the
  "first strong character" heuristic is known to misfire). Requiring every
  node to declare `dir` explicitly, with no auto-detect fallback, would be
  more "explicit" in the abstract but would make plain Latin documents (the
  overwhelming majority, today) carry a mandatory attribute for no reason —
  a worse tradeoff than the web-proven `auto`-default model.

**Resolution mechanism:** when `dir="ltr"` or `dir="rtl"` is explicitly set,
Phase 2 prepends U+200E or U+200F (respectively) to the text passed to the
parley builder for that node and its descendants (until a nested `dir`
overrides again). `dir="auto"` (including the default) prepends nothing —
parley's native auto-detection already runs.

## 2. Logical properties — decided: inline-axis margin/padding + text-align extension only

**Add**, as new per-side properties (none exist today, physical or
logical — see the corrected starting state above):

- `margin-inline-start`, `margin-inline-end`
- `padding-inline-start`, `padding-inline-end`

**Do not add** physical per-side equivalents (`margin-left`/`-right`, etc.)
in this pass. Mizu has no per-side box model at all today; adding four new
physical properties *and* two new logical ones in the same commit is scope
creep beyond what RTL support requires. The existing uniform `margin`/
`padding` (all four sides) remains as the "just size it symmetrically"
convenience. A future prompt can add block-axis (`margin-block-start/end`)
and/or full physical per-side properties if a real need emerges; this memo
deliberately does not pre-build that.

**`text-align: start | end`** — extend the existing `text-align` property
(`left | center | right | justify`, ux-3) with two more values, resolved to
`left`/`right` at paint time based on the node's resolved `dir` (`start` →
`left` under LTR, `right` under RTL; `end` is the mirror). `center` and
`justify` are already direction-agnostic and need no change.

**Resolution rule (shared by both):** `*-inline-start` resolves to the
physical *left* edge under `dir="ltr"`/`auto`-resolved-LTR, and the physical
*right* edge under `dir="rtl"`/`auto`-resolved-RTL; `*-inline-end` is the
mirror. Resolution happens in `layout_bridge::translate_style` (which
already resolves `MizuDimension` — see ux-6's `resolve_dimension` for the
established pattern of "resolve against context, then hand Taffy a plain
physical value") given the node's resolved direction as an added input,
analogous to how ux-6 threads `ViewportSize` through the same function.

**In scope (expanded at sign-off) — flex-container mirroring.** Under
resolved `dir="rtl"`, a `flex-direction: row` container's children must
visually reverse — CSS achieves this by making `flex-direction: row`
direction-relative (it means "row, start-to-end", and start/end flip with
`direction`). Decided mechanism, verified against the vendored Taffy source
(`taffy-0.5.2/src/style/flex.rs`): **`taffy::style::FlexDirection` already
has a `RowReverse` variant** ("items will be added from right to left in a
row" — Taffy's own doc comment, an exact match for what's needed). Phase 2
implements this as a silent substitution in `translate_style`: when a
node's resolved `flex-direction` is `row` **and** its resolved `dir` is RTL,
emit `FlexDirection::RowReverse` to Taffy instead of `Row`. `column` is
unaffected by horizontal text direction and passes through unchanged
regardless of `dir`. The author-facing grammar is untouched — `flex-direction`
still only accepts `row`/`column` (no `row-reverse`/`column-reverse` exist
in the author-facing vocabulary today, so there is no collision between an
author explicitly requesting reverse and this internal substitution; if
`row-reverse` is ever added as an author-facing value, that value's
interaction with `dir` will need its own decision at that time — noted here
so it isn't forgotten, not resolved now since it doesn't exist yet).

Physical per-side properties and block-axis logical properties
(`margin-block-start/end`, etc.) remain **out of scope** — not requested at
sign-off; the reasoning in the original draft (no per-side box model exists
yet at all) still applies to those two items specifically.

## 3. The `direction` naming collision — decided: rename to `flex-direction`

ux-6's memo flagged this and asked ux-7 to pick the non-colliding name.
Decision: **rename the existing flex-axis property from `direction` to
`flex-direction`**, freeing `direction` to later mean what CSS means by it
(`ltr`/`rtl`) if a style-level (rather than attribute-level) spelling is
ever wanted — though per §1, this memo doesn't introduce that property at
all, so `direction` is simply retired from the style vocabulary for now,
not immediately reassigned.

**Why rename rather than introduce a differently-named new property
(`text-direction`, `writing-direction` — ux-6's suggested alternatives) and
leave `direction` as the flex axis:** `flex-direction` is CSS's own actual
name for exactly this concept (`row`/`row-reverse`/`column`/`column-reverse`)
— CSS's `direction` property is the unrelated ltr/rtl concept. Mizu's
current `direction: row` is, today, a false friend for anyone who knows CSS:
the name that exists collides with a *different* CSS property, not merely
sounds similar to one. Renaming to match CSS's real name removes the
confusion permanently instead of just routing around it, and this is the
right moment to do it — before any document depends on the current name
(the same "freeze semantics before users" argument `ROADMAP.md` and prior
memos already make).

**This is a breaking rename, done deliberately, not aliased.** No
`direction`/`flex-direction` dual-accept: Mizu has no external users yet
(per the project's own stated philosophy of freezing names before that's
true, not after), and the codebase has no precedent for accept-both-names
shims anywhere. Phase 2's blast radius: the `apply_property` match arm, the
`StyleRules` field name, `translate_style`'s consumption of it, and every
existing reference in tests/`grammar.md`/`tutorial/index.md`/example
`.mizu` fixtures that currently write `direction row`/`direction column`.
**This is flagged here explicitly because it is the one decision in this
memo most likely to warrant push-back — say so at sign-off if the rename
is unwanted and this memo will use `text-direction` for the new concept
instead, leaving `direction` as the (still confusingly-named) flex axis.**

## 4. Bidi control-character policy (the guardrail)

Two different surfaces, two different, deliberately different rules —
collapsing them into one blanket rule would be wrong in both directions:

- **Document body text: left alone, not stripped.** U+2066–U+2069
  (isolates) in particular are *legitimate, necessary* characters for
  correctly authoring mixed-direction text (e.g. isolating a Latin brand
  name inside an Arabic sentence so the surrounding punctuation reorders
  correctly). Stripping them from arbitrary document content would actively
  break correct multilingual text for the sake of a threat that doesn't
  clearly apply there: Mizu has no free-typed-href-as-clickable-label
  pattern (the classic browser spoofing vector), and `mizu://` navigation
  is resolved through the `urls` registry and `check_navigation`'s
  structural parsing (`MizuUri::parse`), not by trusting rendered link text
  — so a spoofed *label* cannot, by itself, redirect navigation anywhere
  the registry didn't already declare.
- **The chrome URL bar: stripped, at every mutation site.** A URL has no
  legitimate reason to contain a formatting character at all (they're not
  part of RFC 3986's syntax), so stripping here has zero legitimate-use
  cost, and the URL bar is precisely the one surface where a user makes a
  trust decision ("is this the domain I expect") based on what's rendered.
  Strip U+202A–U+202E and U+2066–U+2069 (delete, don't replace with a
  placeholder glyph — a deleted character can't be used to reconstruct a
  different-looking string) at **every** point `ChromeState.url` can be
  written: typed input and paste (`chrome_vello.rs`'s existing
  `is_control()` filter gains this as a second condition) *and*
  programmatic assignment after a successful navigation
  (`navigate_to_url`'s `chrome_state.url = target.clone()` line) — a
  document-driven `navigate` action must not be able to plant an
  override character into the address bar's display any more than typing
  one can. Phase 2 should factor this into one shared helper both call
  sites use, so the two sites can't drift (mirroring why `navigate_to_url`
  itself is the single choke point for navigation policy).

## Security posture

Bidi/RTL is pure text-shaping and layout-mirroring: no capability, no I/O,
no taint, no new sink. The one exception carved out and handled above is
display spoofing via bidi override characters, addressed by stripping them
from the one surface (the URL bar) where visual trust decisions are made.

## Cross-references

- ux-6 (`docs/design/responsive.md`): flagged the `direction` naming
  collision and asked this memo to resolve it — resolved in §3
  (`flex-direction`). ux-6's `@min-width`/`@max-width`/`@dark`/`@light`
  `variant_condition` mechanism is **not** extended with `@rtl`/`@ltr`:
  `dir` is per-node, tree-inherited content metadata, not global
  environment state like window width or OS color scheme, so it's the
  wrong fit for a mechanism that resolves once per layout pass against a
  single global snapshot — see §1's "why an attribute, not a style
  property" for the same underlying distinction.
- ux-3 (`render/text_engine.rs`, `parser/style.rs`): `text-align` is
  extended in place (§2); the font-fallback module doc's "what's
  implemented vs. deferred" note should get a short addendum once Phase 2
  lands, noting base-direction support alongside the existing coverage-bar
  claims.
- ux-2 (`render/accessibility.rs`): out of scope for Phase 2, but the
  accessible-name derivation there reads the same document text this memo's
  §4 analysis covers — worth a follow-up glance once this ships, not a
  blocker now.

## Decisions confirmed at sign-off

1. **`direction` → `flex-direction` rename:** approved as written (§3).
2. **Logical-property scope:** expanded from the draft — inline-axis
   margin/padding, `text-align: start/end`, **and flex-container mirroring**
   (`row` ↔ `RowReverse` under resolved RTL) are all in scope for Phase 2
   (§2). Physical per-side and block-axis logical properties remain out of
   scope.
3. **Control-character policy:** approved as written (§4) — document body
   text unstripped, URL bar stripped at every mutation site.

---

**Phase 1 complete and signed off.** Phase 2 (implementation) proceeds in a
separate commit, implementing exactly the decisions above.
