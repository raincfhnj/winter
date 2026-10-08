# Contributing to Winter

Thanks for your interest in improving Winter. This document explains how to set
up the project, run the quality gate, and submit changes.

## Prerequisites

- Windows 10/11 x64
- Rust stable 1.88 or newer (MSVC toolchain)
- Windows Terminal 1.21+ for manual testing

## Build and test

```powershell
cargo --locked fmt --all -- --check
cargo clippy --all-targets --all-features --locked -- -D warnings
cargo test --all-targets --locked
cargo test --doc --locked
$env:RUSTDOCFLAGS = '-D warnings'; cargo doc --no-deps --locked
cargo build --release --bins --locked
```

All of these must pass before a pull request is merged. CI runs the same commands as
five parallel jobs on `windows-latest` (format, clippy & docs, test, release build,
and an MSRV job that runs `cargo check --all-targets --locked` on Rust 1.88).

Run the controller in the foreground during development:

```powershell
cargo run --bin winter -- run --no-launch
```

Always inspect the read-only plan before installing anything into a real Windows
Terminal configuration:

```powershell
cargo run --bin winter -- plan
cargo run --bin winter -- doctor
```

## Project layout

| Path | Responsibility |
|---|---|
| `src/model.rs` | Directions, actions, terminal channels, window identity |
| `src/config.rs` | TOML schema, defaults, shortcut validation |
| `src/registry.rs` | Canonical action registry; derives the prefix shortcut specs and managed bridge bindings at compile time with `const` consistency checks |
| `src/keys.rs` | Canonical key table; single source for config name parse/display and both virtual-key maps |
| `src/keymap.rs` | `ManagedBinding`/`BridgeChord` types and the registry-derived bridge view (`MANAGED_BINDINGS`) |
| `src/prefix.rs` | Win32-free prefix state machine; shortcut specs derived from the registry |
| `src/pane_layout.rs` | Win32-free divider inference, hit testing, drag steps |
| `src/controller/` | Prefix/pointer reducers, bounded action queue, dispatcher |
| `src/platform/windows/` | Hooks, UI Automation, foreground identity, `SendInput`, elevation |
| `src/integration/` | Terminal discovery, JSONC transactions, backups, manifest, shell integration (split across `discovery`/`targets`/`fragment`/`jsonc`/`transaction`/`rollback`/`manifest`/`helpers`/`shell`/`types`) |
| `src/bin/` | CLI entry points and the hidden daemon |

More detail is in [`docs/DEVELOPMENT.md`](docs/DEVELOPMENT.md) and
[`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md).

## Coding conventions

- Use default `rustfmt` formatting.
- Clippy runs with `-D warnings`; do not add `#[allow]` without a comment explaining why.
- Return `Result` for recoverable errors. Do not `unwrap`/`expect` on production input
  paths; fixtures and tests may.
- Every `unsafe` block must be small and carry a `// SAFETY:` comment stating the
  invariant that makes it sound.
- All handles, hooks, and mutexes must be RAII-managed.
- Hook callbacks must not perform I/O, block, or start processes. Hand work to another
  thread through the bounded queue.
- UI Automation must run on the observer/worker thread, never inside a hook callback.
- Do not persist or invent a pane tree; `PaneLayout` is only re-derived from the latest
  native rectangles.

## Tests

- Prefer pure unit tests for state machines (`prefix`, `pane_layout`, `config`) and for
  the canonical tables (`registry`, `keys`) with no Win32 dependency.
- Use temporary directories and JSONC fixtures for integration tests. Never rewrite a
  real `settings.json` in a test.
- Tests that install global hooks or inject real input live in `tests/live_bridge.rs`
  behind `#[ignore]` and must be run manually on a dedicated test machine.
- Behavior that depends on a real desktop (physical mouse drag, real prefix typing,
  UIPI boundaries) cannot be automated. If your change touches it, describe the manual
  verification you performed in the pull request.

## Commit and pull request guidelines

- Keep commits focused; one logical change per commit.
- Write commit messages in the imperative mood (for example,
  `fix: pass pointer moves through during divider drags`).
- Reference related issues in the PR description.
- Explain any user-visible behavior change and how you verified it.
- Do not commit secrets, machine-specific paths, or generated build artifacts.

## License

By contributing, you agree that your contributions are dual licensed under the MIT
license and the Apache License, Version 2.0, as described in [`README.md`](README.md).
