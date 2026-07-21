# Mizu — Quick start

Mizu reads a `.mizu` file and draws it in a native window.

## Requirements

- [Rust and Cargo](https://rustup.rs/)
- Windows is the main target (`x86_64-pc-windows-msvc`). Linux and macOS are supported in the code but not yet tested.

## Run

From the project root, open a document:

```powershell
cargo run -- ./example.mizu
```

Launched with no arguments, the navigator opens a built-in blank start page
(like a browser's new tab). Type a `mizu://` address in the URL bar to
navigate:

```powershell
cargo run
```

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

## Insecure mode (local server only)

This is for development only, to talk to a local Mizu server without a CA-signed certificate. The TLS bypass applies **only to local hosts** (`localhost` / `127.0.0.1`); for any remote host `--allow-insecure` has no effect and certificate verification stays on.

It takes two opt-ins — one at compile time, one at run time:

```powershell
cargo run --features insecure-dev -- --allow-insecure ./example.mizu
```

- `--features insecure-dev` tells **Cargo** whether to compile the TLS-bypass code into the binary at all. Without it, the bypass simply isn't there, so a normal (release) build can never enable it.
- `--allow-insecure` tells the **program** to actually turn the bypass on for this run. Even a build that has the capability stays secure until you ask for it.

(The `--` is just Cargo's separator: flags before it go to Cargo, everything after goes to Mizu.)
