# Contributing to ORBIT

Thanks for helping. This file covers how to get a working setup, how the
code is laid out, and what a good change looks like.

## Setup

Any of these works:

- **VS Code + dev container (everything preinstalled):** open the folder and choose
  *Reopen in Container*. `.devcontainer/` installs Rust, Node, Python, Go,
  Java, .NET, PostgreSQL and openssl.
- **Locally:** Rust 1.85+ is required. Node 22, Python 3.9+, Go 1.21+, Java 17+
  and .NET 8 are needed only for the SDK you work on.

```bash
cargo build
cargo test
./scripts/check.sh
```

`./scripts/check.sh` runs everything CI runs and skips toolchains you don't
have. In VS Code, the same checks are tasks (*Terminal → Run Task*), and
`.vscode/launch.json` can debug the server or the VS Code extension.

## Layout

| Path | What |
|---|---|
| `crates/orbit-syntax` | lexer, parser, AST, diagnostics, formatter |
| `crates/orbit-ir` | `orbit-ir/1`: types, expressions, invariants |
| `crates/orbit-compiler` | name resolution, type checking, lowering |
| `crates/orbit-runtime` | engine, decider, storage, HTTP/gRPC servers, identities, telemetry |
| `crates/orbit-cli` | the `orbit` binary, including the language server |
| `crates/orbit-worker`, `sdks/*` | worker SDKs (Rust, Python, TypeScript, Go, Java, C#) |
| `protocol/` | the canonical worker protocol (`worker.proto`) |
| `specification/` | RFC-0001 and the grammar |
| `editors/vscode` | the VS Code extension |
| `tests/e2e` | end-to-end tests with real processes |

## Guidelines

- **Semantics come first.** Changes to the language or to runtime behaviour
  must update `specification/` (grammar, RFC appendix) and `docs/`, and must
  include tests: compiler tests for new diagnostics, engine tests for new
  runtime behaviour, e2e tests when processes are involved.
- **Keep the decider pure.** `crates/orbit-runtime/src/decide.rs` must not do
  I/O or read the clock. Every decision has to be reproducible from history.
- **Never weaken durability.** Events are persisted before memory changes,
  and delivery is at-least-once. Don't add shortcuts that skip either.
- **Diagnostics are an API.** Error codes never change meaning. Add new codes
  to `docs/diagnostics.md`.
- **Protocol changes** go into `protocol/orbit/worker/v1/worker.proto` and its
  copy in `crates/orbit-runtime/proto/`. A test checks the two are identical.
  Keep changes backwards compatible within `v1`.
- Run `cargo fmt`, `cargo clippy --all-targets -- -D warnings` and
  `orbit fmt` on any `.orbit` files you touch.

## Pull requests

Keep pull requests focused, explain the *why*, and mention the tests you added.
CI must be green. By contributing you agree that your contributions are
licensed under the Apache License 2.0, the same license as the project.
