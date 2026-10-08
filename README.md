# Winter

[![CI](https://github.com/raincfhnj/winter/actions/workflows/ci.yml/badge.svg)](https://github.com/raincfhnj/winter/actions/workflows/ci.yml)
[![License: MIT OR Apache-2.0](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](#license)
[![Rust 1.88+](https://img.shields.io/badge/rust-1.88%2B-orange.svg)](https://www.rust-lang.org)

**tmux-style keyboard control for Windows Terminal — without replacing it.**

Winter is a headless Rust controller that adds a tmux-like two-stage prefix
(`Ctrl+B`, then a second key) to the Windows Terminal you already use. It never draws
a window, never hosts a terminal, and never manages a PTY. Windows Terminal stays the
only UI, renderer, pane tree, tab manager, and shell owner; Winter only turns
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
"modifiers + one non-modifier key". Winter runs a small low-level keyboard hook
that recognizes the prefix only while a Windows Terminal window is in the foreground,
then injects a hidden single-chord bridge key that is bound to a `User.Winter.*`
action. Your existing profiles, themes, fonts, shells, and keybindings are untouched.

> Note: action ids, on-disk paths, fragment directory, and shell markers are all
> Winter-branded; `winter install` migrates any legacy `WinTerminalP` installation
> automatically.

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

Winter is not limited to tmux parity: planned directions include pane and tab
management, workspace and session-like features, window navigation, and a command
palette, surfaced through a TUI dashboard (`winter ui`) for humans while agents and
automation keep the scriptable CLI.

## Requirements

- Windows 10/11 x64
- Windows Terminal 1.21 or newer
- To build: Rust stable (1.88+) with the MSVC toolchain

The controller runs elevated so it can inject input into both normal and
administrator-elevated Windows Terminal windows. `winter run`, `winter launch`, and
`winterd.exe` self-relaunch through UAC when needed; the read-only/config commands
(`config`, `plan`, `install`, `uninstall`, `doctor`) do not.

## Quick start

```powershell
git clone https://github.com/raincfhnj/winter.git
cd winter
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
| `winterminalp.exe` | Compatibility alias of Winter with the same CLI |
| `winterd.exe` | Hidden background controller (double-clickable) |

Run them from `target\release`, or install them onto your `PATH` with
`cargo install --path . --bins --locked`.

### Upgrading from 0.2.x

Older installs used the historical `WinTerminalP` directories, action ids, and shell
markers. A plain `winter install` detects that legacy installation and removes it
first (old keybindings, fragment, shell block, and state directory), then installs the
Winter-branded integration; your config shortcuts are carried over from
`%LOCALAPPDATA%\WinTerminalP\config.toml` to `%LOCALAPPDATA%\Winter\config.toml`.
Manual equivalent: run the OLD version's `winter uninstall`, delete
`%LOCALAPPDATA%\WinTerminalP`, then run `winter install`.

## Usage

| Command | What it does |
|---|---|
| `winter` / `winter launch` | Start the background controller and open Windows Terminal; the bridge is installed automatically on first run |
| `winter run [--no-launch]` | Run the controller in this process; `--no-launch` keeps it from opening a new window |
| `winter ui [--once]` | Live pane dashboard in this terminal — the humans' TUI surface; `--once` prints a single frame for scripts |
| `winter config` (`--path` / `--edit`) | Print the effective config, print only its path, or open it in Notepad |
| `winter plan` / `winter install` / `winter uninstall` | Bridge lifecycle: preview the changes, install the fragment, hidden keybindings, and shell block, or remove only what Winter still owns |
| `winter doctor` | Machine-readable JSON `{schema_version, healthy, config:{path, ok, error}, integration}` |

Open `winter ui` in a pane to watch panes, focus, and prefix state live; `q`, `Q`,
`Ctrl+C`, or `Esc` quits, and the dashboard shows `OFFLINE` when the controller is not
running.

Exit statuses are listed under [Exit codes](#exit-codes). Scripts and agents should
prefer the machine-readable outputs — `winter ui --once` and the JSON reports printed
by `plan` / `install` / `uninstall` / `doctor` — over the interactive TUI.

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

The file lives at `%LOCALAPPDATA%\Winter\config.toml`. Only the actions you want
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
Winter.

## How it works

The project is a small modular Rust monolith. Pure state machines (`prefix`,
`pane_layout`) have no Win32 dependency; a narrow `platform/windows` adapter owns the
low-level hooks, UI Automation, foreground identity, and `SendInput`; `integration`
owns lossless JSONC transactions and backups. Behavior is single-sourced from two
canonical tables: the `registry` module derives the prefix shortcut specs and the
managed bridge bindings at compile time, and the `keys` module drives key-name
parsing plus both virtual-key maps. See
[`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md) for the full design and failure
semantics.

## Documentation

- [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md) — architecture and failure semantics
- [`docs/SHORTCUTS.md`](docs/SHORTCUTS.md) — keybinding reference
- [`docs/DEVELOPMENT.md`](docs/DEVELOPMENT.md) — build and development guide
- [`docs/TESTING.md`](docs/TESTING.md) — test strategy and manual verification
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
cargo --locked fmt --all -- --check
cargo clippy --all-targets --all-features --locked -- -D warnings
cargo test --all-targets --locked
cargo test --doc --locked
$env:RUSTDOCFLAGS = '-D warnings'; cargo doc --no-deps --locked
cargo build --release --bins --locked
```

CI runs these as five parallel jobs on `windows-latest`: format, clippy & docs, test,
release build, and an MSRV job that checks Rust 1.88 with
`cargo check --all-targets --locked`.

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- MIT license ([LICENSE-MIT](LICENSE-MIT))

at your option.

Unless you explicitly state otherwise, any contribution intentionally submitted for
inclusion in this work, as defined in the Apache-2.0 license, shall be dual licensed as
above, without any additional terms or conditions.
