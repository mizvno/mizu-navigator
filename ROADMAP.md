# Mizu — Remediation roadmap

Execution map for the six work prompts derived from the project's security/architecture
review. Each prompt is a self-contained brief for an implementation agent (e.g. Claude Code)
with its own verified anchors, plan, tests, and acceptance criteria. **This file is the
orchestration layer: order, dependencies, and coverage.**

Guiding thread: the report's root finding is that Mizu enumerates *static* capabilities well
but does not constrain the *dynamic flows* that start from them, and enforces security by a
growing denylist rather than a verified invariant. The sequence below moves from **stop the
bleeding → bound resources → close minor leaks → prove the class is empty → freeze semantics
→ write the missing spec.**

## Execution order (strictly sequential)

Run one at a time, in this order. Do **not** parallelize: several prompts touch the same
files (`src/render/window.rs`, `src/parser/logic.rs`), and prompts 4 and 6 consume artifacts
the earlier ones produce. After each, verify against that prompt's Acceptance Criteria before
starting the next.

| # | Prompt | Report priority | Fixes | Key files |
|---|--------|-----------------|-------|-----------|
| 1 | `PROMPT-navigation-policy.md` | HIGH | Redirect → navigation escalation; `navigate <expr>`; non-uniform scheme filter. Single `NavigationPolicy` choke point. | `network/worker.rs`, `render/window.rs`, new `render/navigation.rs` |
| 2 | `PROMPT-layout-budget.md` | HIGH | Unbounded `each` expansion (remote-data DoS); nested `each`. Node budget + observable truncation. | `render/layout_bridge.rs`, `render/window.rs`, `parser/layout.rs` |
| 3 | `PROMPT-tactical-hardening.md` | HIGH + MEDIUM | `path_param` injection; remote relative `image src`; storage write-only declared. | `parser/logic_worker.rs`, `parser/logic.rs`, `parser/layout.rs`, `core/storage.rs` |
| 4 | `PROMPT-flow-checker.md` | MEDIUM (highest ROI) | Replaces `SIDE_EFFECT_BUILTINS` denylist with a sound load-time taint checker. Emits `SECURITY-INVARIANTS.md`. | new `parser/flow.rs`, `parser/logic.rs`, `render/inspector/` |
| 5 | `PROMPT-semantics-freeze.md` | LOW urgency, time-sensitive | Numeric model, concurrent last-writer, timer bounds, list cliff. Emits `SEMANTICS.md`. | `parser/logic.rs`, `render/window.rs`, `network/worker.rs`, `core/types.rs` |
| 6 | `PROMPT-language-reference.md` | MEDIUM (gravest doc gap) | The absent independent spec: EBNF grammar + semantics + tutorial, machine-checked. | `docs/**`, `tests/` |

## Dependencies

```
1 navigation ─┐
2 layout ─────┼─→ 4 flow-checker ─→ 6 language-reference
3 tactical ───┘                          ↑
5 semantics ─────────────────────────────┘
```

- **4 depends on 1 + 3.** The flow checker's *sinks* (navigation target, `path_param`) and
  its *gates* (navigation choke point, `path_param` validation) are defined by 1 and 3. It
  also *replaces* the denylist those touch. Run 4 only after 1–3.
- **6 depends on all.** The reference describes decided behavior and cross-links
  `SECURITY-INVARIANTS.md` (from 4) and `SEMANTICS.md` (from 5). Run it last.
- **5 is independent** of 1–4 but should precede 6 so the reference documents frozen numerics.
- **1, 2, 3 are mutually independent** in logic but share files — still run sequentially to
  keep diffs reviewable and avoid merge friction.

Why time-sensitivity flips the priority on 5: it changes observable program *results*
(division type, concurrent resolution). With **zero deployed documents** these are free to
change now; after the first stranger's document depends on them they are frozen by
compatibility. Do 5 before Mizu has users, regardless of its "low" report ranking.

## Report §8 coverage

- **HIGH** (all): redirect revalidation → 1; `each` limit → 2; constrain `navigate` → 1;
  `path_param` encoding → 3.
- **MEDIUM**: capability/flow spec + taint checker replacing the denylist → 4; relative
  `image src` → 3; publish spec/tutorial → 6; Kani/Creusot on kernel functions → *follow-up*.
- **LOW**: storage write-only documented → 3; numeric/timer/cliff semantics → 5; `comp`
  over-approximation & concurrency clarified → 5 + 6; type-system hardening → *follow-up*;
  mechanized semantics → *follow-up*.

## Emitted artifacts (durable, versioned in-repo)

- `SECURITY-INVARIANTS.md` — from 4. The single index of capability/flow/navigation/layout/
  storage invariants, each with enforcement location. Becomes the spec input for Kani.
- `SEMANTICS.md` — from 5. The four frozen semantic decisions with rationale.
- `docs/reference/grammar.md`, `docs/reference/semantics.md`, `docs/tutorial/index.md` — from
  6. The independent, machine-checked language reference.

## Follow-ups (after the six; not yet briefed)

1. **Kani / Creusot verification** of the ~10 security-kernel functions (`parse_urls`,
   `resolve_endpoint_url`, `is_local_host`, `file_sandbox_contains`, `MizuUri::parse`,
   redirect parsing, `check_storage_write`, plus the new `navigation.rs` verdict and
   `flow.rs` checker). **Gated on 4** — consumes `SECURITY-INVARIANTS.md` as its property
   spec. Highest remaining assurance-per-cost after the six.
2. **Type-system hardening** — type `Record`/`Null`, annotate parameters, so a soundness
   statement (report §4a) becomes *expressible*. Low urgency: closes no hole, enables a
   future theorem.
3. **Mechanized formal semantics** (Lean/Coq) — long-term. Gated on 6 (the human-readable
   reference is its input). The endgame the report points at, not urgent.

## Working agreement for each prompt

Each brief already specifies this, repeated here as the standing rule:
- Explore and confirm every cited anchor before changing code (line numbers may have drifted).
- `cargo test --lib` and `--features insecure-dev` green; `cargo clippy --lib --bins` clean;
  `missing_docs` clean. One commit per logical unit.
- Fail-secure: on any uncertainty, reject/limit rather than widen a capability.
- Stay in the prompt's scope; note discovered issues as follow-ups instead of expanding the diff.
