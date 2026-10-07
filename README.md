# WinTerminalP

[![CI](https://github.com/raincfhnj/winterminalp/actions/workflows/ci.yml/badge.svg)](https://github.com/raincfhnj/winterminalp/actions/workflows/ci.yml)
[![License: MIT OR Apache-2.0](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](#license)
[![Rust 1.85+](https://img.shields.io/badge/rust-1.85%2B-orange.svg)](https://www.rust-lang.org)

**tmux-style keyboard control for the native Windows Terminal — without replacing it.**

WinTerminalP is a headless Rust controller that adds a tmux-like two-stage prefix
(`Ctrl+B`, then a second key) to the Windows Terminal you already use. It never draws
a window, never hosts a terminal, and never manages a PTY. Windows Terminal stays the
only UI, renderer, pane tree, tab manager, and shell owner; WinTerminalP only turns
prefix chords into native Windows Terminal actions.

```text
Ctrl+B, Shift+Right   →  split pane to the right
Ctrl+B, ←/→/↑/↓       →  move focus
Ctrl+B, Ctrl+→        →  resize pane
Ctrl+B, C / N / P     →  new / next / previous tab
Ctrl+B, X / Z / ,     →  close pane / zoom / rename tab
```

## Why

Windows Terminal has no tmux-style prefix mode, and its keybindings can only express
"modifiers + one non-modifier key". WinTerminalP runs a small low-level keyboard hook
that recognizes the prefix only while a Windows Terminal window is in the foreground,
then injects a hidden single-chord bridge key that is bound to a `User.WinTerminalP.*`
action. Your existing profiles, themes, fonts, shells, and keybindings are untouched.

## Features

- **Two-stage prefix** — default `Ctrl+B`; fully configurable, including timeout.
- **Panes** — split, focus, and resize in all four directions.
- **Tabs** — new, next/previous, index activation (`0`–`9`), and native rename.
- **Mouse divider drag** — drag a native pane divider; geometry is inferred from
  disposable UI Automation rectangles and translated to native `resizePane` steps.
- **Directory inheritance** — installs a managed `OSC 9;9` PowerShell prompt wrapper so
  duplicated panes open in the same working directory.
- **Safe, reversible install** — lossless JSONC editing that preserves comments,
  ordering, indentation, and trailing commas; raw-byte backups, SHA-256 compare-and-swap,
  and an uninstall that keeps anything you edited.
- **Transparent by default** — keys pass through to every non-Terminal foreground app,
  and injected input never activates the prefix.
- **No telemetry** — no network access, no terminal-buffer or keystroke logging.

## Requirements

- Windows 10/11 x64
- Windows Terminal 1.21 or newer
- To build: Rust stable (1.85+) with the MSVC toolchain

The controller runs elevated so it can inject input into both normal and
administrator-elevated Windows Terminal windows. `winter run`, `winter launch`, and
`winterd.exe` self-relaunch through UAC when needed; the read-only/config commands
(`config`, `plan`, `install`, `uninstall`, `doctor`) do not.

## Quick start

```powershell
git clone https://github.com/raincfhnj/winterminalp.git
cd winterminalp
.\install.ps1   # build, install the `winter` command, and set up the integration
winter          # start the controller (Windows prompts for UAC)
```

`install.ps1` is a one-time step: it builds the release binaries with
`cargo install`, puts `winter.exe` in the Cargo bin directory (already on `PATH`),
and installs the Windows Terminal integration. After that, `winter` works from any
shell and keeps working across reboots — just run `winter` again after a restart.
If the integration is ever missing, `winter` reinstalls it automatically on first
launch.

Press `Ctrl+B` followed by a second key to act. Press `Ctrl+B`, `Q` to stop the
controller without closing Windows Terminal.

### Manual build

```powershell
cargo build --release --bins
```

This produces three binaries in `target\release`:

| Binary | Purpose |
|---|---|
| `winter.exe` | Recommended command entry point |
| `winterminalp.exe` | Compatibility alias with the same CLI |
| `winterd.exe` | Hidden background controller (double-clickable) |

Run them from `target\release`, or install them onto your `PATH` with
`cargo install --path . --bins --locked`.

## Default keybindings

All shortcuts work only while Windows Terminal is in the foreground. Press and release
`Ctrl+B`, then press the second key.

| Second key | Action |
|---|---|
| `←` / `→` / `↑` / `↓` | Focus the pane in that direction |
| `Shift` + arrow | Split a new pane in that direction |
| `Ctrl` + arrow | Resize the active pane in that direction |
| `C` | New tab |
| `N` / `P` | Next / previous tab |
| `0`–`9` | Activate the zero-based tab index |
| `X` | Close the active pane |
| `Z` | Toggle pane zoom |
| `,` | Rename the current tab |
| `B` | Send a literal prefix (default `Ctrl+B`; follows a custom prefix) |
| `Q` | Stop the controller (does not close Windows Terminal) |
| `Escape` | Cancel the prefix |

## Mouse resize

Move the pointer over a native Windows Terminal divider; the cursor changes to the
horizontal or vertical resize shape. Hold the left button and drag to resize the
adjacent panes. Sizes change in Windows Terminal's native ~5% parent-split steps.

```toml
[mouse_resize]
enabled = true
divider_hit_slop_px = 8
geometry_poll_interval_ms = 100
```

## Configuration

```powershell
winter config          # print the full effective config and its path
winter config --path   # print only the path
winter config --edit   # open it in Notepad
```

The file lives at `%LOCALAPPDATA%\WinTerminalP\config.toml`. Only the actions you want
to override need to be present; everything else keeps its default. For example, a
Vim-style layout:

```toml
prefix = "ctrl+a"

[shortcuts]
focus_left = "h"
focus_down = "j"
focus_up = "k"
focus_right = "l"
new_tab = "t"
shutdown = "q"
```

The prefix must include `Ctrl` or `Alt`. `Escape` and reserved system combinations
(`Alt+Tab`, `Ctrl+Tab`, `Ctrl+Shift+Tab`, `Alt+F4`, `Alt+Escape`, `Ctrl+Escape`,
`Ctrl+Shift+T`, `Ctrl+Shift+W`, Windows-key chords) cannot be bound. Two actions
cannot share a chord. Optional actions may be set to `"disabled"`. Shortcut changes do
not require re-running `winter install`.

All configuration keys, defaults, and accepted ranges:

| Key | Default | Range / notes |
|---|---|---|
| `schema_version` | `2` | Config schema; legacy `1` files load and are migrated in memory |
| `prefix_timeout_ms` | `1500` | `250`–`5000`; prefix expiry |
| `launch_terminal_on_start` | `true` | Boolean; open a native Windows Terminal window when the controller starts |
| `prefix` | `"ctrl+b"` | Must include `ctrl` or `alt` |
| `shortcuts` | see above | Map of action name → chord or `"disabled"` |
| `mouse_resize.enabled` | `true` | Boolean; enable native divider dragging |
| `mouse_resize.divider_hit_slop_px` | `8` | `0`–`32` |
| `mouse_resize.geometry_poll_interval_ms` | `100` | `50`–`1000` |

See [`docs/SHORTCUTS.md`](docs/SHORTCUTS.md) for the full reference.

## Exit codes

`plan`, `install`, `uninstall`, and `doctor` print a JSON report on stdout;
`winter doctor` returns `{ schema_version, healthy, config: { path, ok, error },
integration }`.

| Code | Meaning |
|---|---|
| `0` | Success; the command finished and the result is healthy |
| `1` | Hard failure (I/O error, unsupported configuration, ...) |
| `2` | Needs attention: `winter plan` when the bridge is not installable; `winter doctor` when the configuration is invalid or the bridge is not ready |

## Safety and privacy

- The keyboard hook does not record, store, or transmit keystrokes.
- The mouse hook only tests whether the cursor is near a cached divider; no trajectory
  is stored or sent.
- Injected (synthetic) input is always passed through and never re-enters the prefix
  state machine.
- The controller opens no network port and reads no terminal buffer.
- Install backs up the raw bytes before writing and uses compare-and-swap; uninstall
  removes only entries that still match the managed manifest and reports anything you
  changed.

## Uninstall

```powershell
target\release\winter.exe uninstall
```

This removes only the fragment, hidden keybindings, and shell block still owned by
WinTerminalP.

## How it works

The project is a small modular Rust monolith. Pure state machines (`prefix`,
`pane_layout`) have no Win32 dependency; a narrow `platform/windows` adapter owns the
low-level hooks, UI Automation, foreground identity, and `SendInput`; `integration`
owns lossless JSONC transactions and backups. See
[`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md) for the full design and failure
semantics.

## Documentation

- [`docs/PRD.md`](docs/PRD.md) — product requirements
- [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md) — architecture and failure semantics
- [`docs/SHORTCUTS.md`](docs/SHORTCUTS.md) — keybinding reference
- [`docs/DEVELOPMENT.md`](docs/DEVELOPMENT.md) — build and development guide
- [`docs/TESTING.md`](docs/TESTING.md) — test strategy and manual verification
- [`docs/VERIFICATION.md`](docs/VERIFICATION.md) — real-environment verification record
- [`docs/IMPLEMENTATION_PLAN.md`](docs/IMPLEMENTATION_PLAN.md) — implementation plan
- [`docs/fault-reviews/`](docs/fault-reviews) — post-mortems

## Limitations

- Windows Terminal does not expose a pane tree or an action-execution receipt, so pane
  geometry is inferred from visible `TermControl` rectangles and `SendInput` success
  only proves the event was inserted.
- Resizing follows Windows Terminal's ~5% native steps, not arbitrary pixel sizes.
- The project is Windows-only.

## Contributing

Contributions are welcome. Please read [`CONTRIBUTING.md`](CONTRIBUTING.md) first, and
run the quality gate before opening a pull request:

```powershell
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets
```

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- MIT license ([LICENSE-MIT](LICENSE-MIT))

at your option.

Unless you explicitly state otherwise, any contribution intentionally submitted for
inclusion in this work, as defined in the Apache-2.0 license, shall be dual licensed as
above, without any additional terms or conditions.
