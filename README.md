# Mizu

**A hypermedia format and native renderer where a document you've never seen is safe to open.**

Mizu reads a `.mizu` file, its own small, declarative language for describing
a page and how it reacts, and draws it in a native window. It is not a
general-purpose programming language and doesn't aim to reimplement what
HTML5 and JavaScript already do. It aims at one thing instead: every reaction
a document can make is finite, runs only code that shipped with it, and
reaches only the network addresses it declared up front — all of which you
can see before you let it do anything. See [`MANIFESTO.md`](MANIFESTO.md) for
the full case, and [`SECURITY-INVARIANTS.md`](SECURITY-INVARIANTS.md) for the
enumerated guarantees this is built to hold.

[![Kani Verification](https://github.com/mizvno/mizu-navigator/actions/workflows/kani.yml/badge.svg)](https://github.com/mizvno/mizu-navigator/actions/workflows/kani.yml)
[![License: GPL v3](https://img.shields.io/badge/License-GPLv3-blue.svg)](LICENSE)

> **⚠ Pre-1.0.** Mizu is still evolving — the language, the format, and the
> APIs can change in breaking ways without notice. Nothing here is stable yet.

---

## Contents

- [Requirements](#requirements)
- [Quick start](#quick-start)
- [The language, in brief](#the-language-in-brief)
- [Inspector](#inspector)
- [Configuration](#configuration)
- [Insecure mode](#insecure-mode-local-server-only)
- [Formal verification](#formal-verification)
- [Documentation map](#documentation-map)
- [License](#license)

---

## Requirements

- [Rust and Cargo](https://rustup.rs/) — the repo pins its edition/toolchain via `Cargo.toml`; no separate install step beyond `rustup` is needed to build or run.
- **Windows** (`x86_64-pc-windows-msvc`) is the primary, tested target.
- **Linux and macOS** are supported in the source (no platform-specific `unsafe` in the main crate; windowing/rendering go through `winit`/`vello`/`wgpu`, which are cross-platform) but are not yet part of the regular test loop — treat them as "should work," not "verified."
- Formal verification (Kani) is Linux-only regardless of what platform you build the app on — see [Formal verification](#formal-verification).

## Quick start

From the project root, open a document:

```powershell
cargo run -- ./docs/reference/examples/showcase.mizu
```

`docs/reference/examples/` has a graduated set of `.mizu` files, from a
one-line `01_minimal.mizu` up through `showcase.mizu` (a single-file tour of
most of the language). [`docs/tutorial/index.md`](docs/tutorial/index.md)
walks through the same ground step by step if you'd rather read first.

Launched with no arguments, the navigator opens a built-in blank start page
(like a browser's new tab). Type a `mizu://` address in the URL bar to
navigate:

```powershell
cargo run
```

## The language, in brief

A `.mizu` document is four optional blocks — `urls`, `logic`, `style`,
`layout` — parsed in that order, then bound together into a live document:

- **`urls`** declares every network endpoint and media alias the document
  will ever touch, by name, up front. Nothing reaches the network through an
  address that wasn't declared here.
- **`logic`** declares variables, pure functions (checked acyclic — no
  recursion, so every reaction provably ends), computed/derived values, and
  recurring timers.
- **`style`** is a CSS-like cascade: tag/class selectors, a fixed and
  intentionally small property set, and environment-gated variants
  (`@dark`, `@min-width 600`, …).
- **`layout`** is the DOM: `doc`, `box`, `text`, `button`, `input`,
  `image`, `form`, `each`, conditional classes, and `click`/`submit` actions
  that can assign state, navigate, or call a declared endpoint.

The full grammar (with the exact EBNF and every parser-enforced constraint)
is in [`docs/reference/grammar.md`](docs/reference/grammar.md); the
evaluation model (fixed-point arithmetic, budgets, the flow/taint checker)
is in [`docs/reference/semantics.md`](docs/reference/semantics.md).

## Inspector

Press **F12** to toggle the built-in inspector, docked on the right. It is
read-only and shows both what the document *declared* and what it is *doing*:

- **Elem** — document tree; click to select (the element is highlighted in
  the page). The `[+]` button enables the element picker: click anything in
  the page to select it without triggering its action (Esc cancels).
- **Style** — box metrics and the style cascade of the selection, including
  conditional classes with their live on/off state.
- **Logic** — live variables (recent changes flash), computed bindings with
  their dependencies, and function signatures.
- **Events** — declared timers with a countdown to the next tick, declared
  click/submit actions, and the runtime event log.
- **Net** — declared endpoints, storage quota usage, and the request log with
  outcome, duration and size; requests blocked by a policy are marked
  `BLOCKED` with the reason.

Event and network logs are always on (bounded ring buffers), so the inspector
can be opened after a problem and still show its history.

## Configuration

Operational settings (network timeouts, connection pool size, storage write
batching, redirect budget, the QUIC port) can be tuned via an optional TOML
file — `%APPDATA%\mizu\config.toml` on Windows, `$XDG_CONFIG_HOME/mizu/config.toml`
(or `~/.config/mizu/config.toml`) on Linux/macOS. No file, or a file that only
sets some fields, is fine — anything unset keeps its built-in default. See
the field list and defaults in the doc comment at the top of
`src/core/config.rs`.

A handful of evaluator/layout budget constants whose starting values were
picked as reasonable guesses rather than measured — not the security-critical
ones — can be overridden for a single run via `MIZU_*` environment variables
(e.g. `MIZU_MAX_COMP_BINDINGS=2000 cargo run -- ./big.mizu`); see the same
doc comment for the full list. Neither mechanism can touch the fixed
security invariants (`MAX_EVAL_DEPTH`, storage quotas, image/response-body
limits, ...) — see [`SECURITY-INVARIANTS.md`](SECURITY-INVARIANTS.md).

## Insecure mode (local server only)

This is for development only, to talk to a local Mizu server without a CA-signed certificate. The TLS bypass applies **only to local hosts** (`localhost` / `127.0.0.1`); for any remote host `--allow-insecure` has no effect and certificate verification stays on.

It takes two opt-ins — one at compile time, one at run time:

```powershell
cargo run --features insecure-dev -- --allow-insecure ./docs/reference/examples/04_urls_fetch.mizu
```

- `--features insecure-dev` tells **Cargo** whether to compile the TLS-bypass code into the binary at all. Without it, the bypass simply isn't there, so a normal (release) build can never enable it.
- `--allow-insecure` tells the **program** to actually turn the bypass on for this run. Even a build that has the capability stays secure until you ask for it.

(The `--` is just Cargo's separator: flags before it go to Cargo, everything after goes to Mizu.)

## Formal verification

Mizu's security posture isn't only enforced at runtime and covered by tests
(`cargo test`, workspace-wide) — two independent, complementary layers of
formal verification sit on top of it, at different points in the stack.

### Kani (Rust model checking) — **Linux only**

[`crates/core`](crates/core) is split out as its own dependency-light
workspace member specifically so [Kani](https://model-checking.github.io/kani/)
(bounded model checking via CBMC) can verify real Rust functions from the
runtime without ever having to compile the GUI stack (`parley` → `fontconfig`
and friends).

**Platform:** Kani/CBMC only runs on Linux. On Windows, use
[WSL2](https://learn.microsoft.com/windows/wsl/) — that's exactly how this
was developed and validated; there is no native Windows or macOS support.
CI (`.github/workflows/kani.yml`, badge above) runs it on `ubuntu-latest` on
every push and pull request.

To run it yourself (Linux or WSL2):

```bash
cargo install --locked kani-verifier
cargo kani setup
cd crates/core
cargo kani
```

This currently verifies 3 harnesses over `check_type` (parser/logic/eval.rs)
— the runtime function that enforces Phase B's static type contracts — in
about 13 seconds total, 0 failures. That number is deliberately small and
growing carefully rather than fast: `Value`/`ValueType`/`Expr` are
self-referential Rust enums, and several harness shapes that look reasonable
hang CBMC indefinitely for reasons that have nothing to do with correctness
(documented in detail in that module's own doc comment, for whoever picks
this up next).

### Lean 4 (mechanized proof) — cross-platform

[`formal/`](formal) is a separate `λ_mizu` development in Lean 4
(toolchain pinned in `formal/lean-toolchain`, built with `lake`) that proves
properties about a *model* of Mizu's semantics — termination (every reaction
is bounded), and non-interference (the flow checker's `accept` implies no
untrusted value reaches a sink ungated). See
[`formal/RESULTS.md`](formal/RESULTS.md) for the theorem statements and
status, and [`formal/FIDELITY.md`](formal/FIDELITY.md) for the honest,
itemized ledger of every place the model idealizes or diverges from the
shipped Rust — a proof about a model is only as trustworthy as that ledger.

```bash
cd formal
lake build
```

**These two layers answer different questions and neither substitutes for
the other:** Lean proves the *design* is sound as a mathematical object; Kani
is starting to prove the *shipped code* matches specific pieces of that
design. `formal/FIDELITY.md` is the bridge between them.

## Documentation map

| Doc | Covers |
|---|---|
| [`MANIFESTO.md`](MANIFESTO.md) | Why Mizu exists, in four sentences |
| [`SECURITY-INVARIANTS.md`](SECURITY-INVARIANTS.md) | Every enumerated security invariant, its source, and its enforcement mechanism |
| [`docs/tutorial/index.md`](docs/tutorial/index.md) | Learn the language by example, progressively |
| [`docs/reference/grammar.md`](docs/reference/grammar.md) | Full EBNF grammar with parser-enforced constraints, cited to source |
| [`docs/reference/semantics.md`](docs/reference/semantics.md) | Evaluation model: numeric representation, budgets, the flow/taint checker |
| [`docs/design/`](docs/design) | Design memos for individual features (responsive layout, bidi/RTL, the type system) |
| [`formal/RESULTS.md`](formal/RESULTS.md) / [`formal/FIDELITY.md`](formal/FIDELITY.md) | What's proved in Lean 4, and exactly how faithfully the model matches the runtime |

## License

[GPL-3.0-only](LICENSE).
