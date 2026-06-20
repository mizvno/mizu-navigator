# Mizu — Quick start

Mizu reads a `.mizu` file and draws it in a native window.

## Requirements

- [Rust and Cargo](https://rustup.rs/)
- Windows is the main target (`x86_64-pc-windows-msvc`). Linux and macOS are supported in the code but not yet tested.

## Run a file

From the project root:

```powershell
cargo run -- ./example.mizu
```

## Insecure mode (local server only)

This is for development only, to talk to a local Mizu server without a CA-signed certificate. The TLS bypass applies **only to local hosts** (`localhost` / `127.0.0.1`); for any remote host `--allow-insecure` has no effect and certificate verification stays on.

It takes two opt-ins — one at compile time, one at run time:

```powershell
cargo run --features insecure-dev -- --allow-insecure ./example.mizu
```

- `--features insecure-dev` tells **Cargo** whether to compile the TLS-bypass code into the binary at all. Without it, the bypass simply isn't there, so a normal (release) build can never enable it.
- `--allow-insecure` tells the **program** to actually turn the bypass on for this run. Even a build that has the capability stays secure until you ask for it.
