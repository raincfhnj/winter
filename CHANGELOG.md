# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- `winter doctor` now prints the stable JSON contract
  `{ schema_version, healthy, config: { path, ok, error }, integration }`, and
  the CLI documents exit codes: `0` success/healthy, `1` hard failure, `2`
  needs attention (`winter plan` when the bridge is not installable,
  `winter doctor` when the configuration is invalid or the bridge is not
  ready).
- CLI contract tests (`tests/cli.rs`) covering `winter --help` and the
  `doctor` report, with `LOCALAPPDATA` redirected to a temporary directory.
- CI: a dedicated MSRV job on Rust 1.88 (`cargo check --all-targets
  --locked`), `--locked` on every cargo command, a separate
  `cargo test --doc --locked` step, `cargo doc --no-deps` with
  `RUSTDOCFLAGS=-D warnings`, and `concurrency.cancel-in-progress`.
- Exit-code and configuration-key reference sections in `README.md` and
  `README.zh-CN.md`.

### Fixed

- Fragment ownership records survive external reformatting: a semantically
  unchanged fragment no longer drops its manifest record, so `winter uninstall`
  can still remove it instead of reporting `Missing`.
- Unmanaged malformed `keybindings` entries are reported as warnings instead of
  blocking `install`/`uninstall` for every channel; literal `+` chords
  (`ctrl++`) parse as the `plus` key.
- Failed atomic writes no longer leave `.winterminalp-tmp-*.json` staging files
  (or mismatched backups) in Windows Terminal's directories.
- A single unreadable or symlinked `settings.json` no longer aborts
  installation for all channels; symlinked settings files are not discovered.
- A missed key-up can no longer swallow the next keystroke in any application,
  and a stale shutdown handshake cannot fire minutes later on an unrelated
  keypress.
- `winterd` records panics in `last-error.log` (with a `%TEMP%` fallback) and
  exits instead of failing invisibly; `winter doctor` diagnoses invalid
  configurations instead of aborting before any report.

### Changed

- Action metadata now lives in a single registry (`src/registry.rs`): the
  prefix shortcut specs and the managed bridge bindings are derived from one
  const table, with compile-time assertions replacing the runtime count checks
  that previously kept two hand-written tables in sync.
- Key names, display spelling, and the forward/reverse virtual-key maps are
  unified in one canonical table (`src/keys.rs`), guarded by compile-time
  uniqueness checks and whole-table round-trip/inverse property tests.
- The `integration` module is split into `fragment`, `targets`, `rollback`,
  and `helpers` submodules; the public `plan`/`install`/`uninstall`/`doctor`
  surface is unchanged.
- `winterd` parses arguments with clap: `--help`/`--version` now work, and
  invalid daemon arguments are logged to `last-error.log` with exit 1 exactly
  as before.
- The error model is unified: `AppError::Platform` preserves the
  `PlatformError` source chain, `OperationIncomplete` distinguishes tool-side
  incompleteness from user-resolvable `SettingsConflict`, and the action
  worker uses a typed `WorkerError` while keeping its stringly report fields.
- Input dispatch re-validates the foreground window identity immediately
  before `SendInput`, closing the gap between validation and injection.
- Low-level hooks are supervised by a panic/shutdown watchdog that fail-opens
  the hooks instead of leaving them installed.
- Fragment ownership records are retained whenever the fragment still matches
  what we installed (by hash or by semantic equivalence), and are never adopted
  for files we did not create.
- `plan` and `doctor` validate keybindings through a read-only analysis pass
  instead of building and discarding a full serialized replacement document on
  every launch.
- `winter uninstall` is transactional: removals run through the same
  CAS-protected rollback stack as install, and the manifest is retained when
  only some entries could be removed.
- Path matching for ownership records is case- and separator-insensitive
  (`path_key`), so `LOCALAPPDATA` casing changes no longer orphan manifest
  entries.
- Installation is resilient per channel: one failing settings target no
  longer aborts the remaining channels, and completed work is rolled back
  per target.
- The controller resynchronizes physical modifier state when hook observation
  and key state disagree, so a desynced modifier can no longer leak into the
  prefix state machine.
- Prefix sessions clear their suppressed-key ledger on every exit (timeout,
  cancel, foreground change, pointer interaction) and attribute expiry to
  `CancelReason::Timeout`.
- Configuration loading validates schema version, timeout range, mouse-resize
  ranges, and duplicate/reserved chords before start-up; config files are
  written atomically (create-new + fsync).
- `install.ps1` reloads the persisted User/Machine PATH from the registry,
  locates `winter.exe` via `Get-Command` with a `cargo install --list`
  fallback, verifies both `winter.exe` and `winterd.exe`, and checks the
  persisted PATH instead of the process PATH.
- Reserved system chords now include `Ctrl+Tab`, `Ctrl+Shift+Tab`,
  `Ctrl+Shift+T`, and `Ctrl+Shift+W`, matching Windows Terminal's defaults
  and `is_reserved_system_chord`.

- The MSRV is raised to Rust 1.88: `jsonc-parser` 0.33 uses let-chains,
  which only compile from 1.88, so the previously declared 1.85 could never
  have built the dependency graph (the claim was unverified until the CI MSRV
  job was added).

### Breaking

- The JSON report shapes of `doctor`, `install`, and `uninstall` changed
  (doctor nests `config` and `integration`; install/uninstall report
  per-target status and retained entries), along with the exit codes above.
- `PaneDivider` fields are now private; construct via `PaneDivider::new` and
  read through accessor methods.
- The reserved system chord set grew (`Ctrl+Tab`, `Ctrl+Shift+Tab`,
  `Ctrl+Shift+T`, `Ctrl+Shift+W`); configurations binding them are now
  rejected at load time.

## [0.2.0] - 2026-09-09

Initial public release. WinTerminalP was rewritten from an abandoned Tauri/xterm
terminal into a headless controller that enhances the native Windows Terminal.

### Added

- tmux-style two-stage prefix (`Ctrl+B`, then a second key) active only while Windows
  Terminal is in the foreground.
- Four-direction pane split, focus, and resize, plus new/next/previous/index tab
  activation, close pane, zoom, and native tab rename.
- Mouse divider dragging inferred from disposable UI Automation rectangles and
  translated to native `resizePane` steps.
- TOML configuration for the prefix, timeout, and every action chord, plus mouse resize
  tuning.
- Lossless JSONC integration: an action fragment, hidden bridge keybindings, raw-byte
  backups, SHA-256 compare-and-swap writes, a committed manifest, and reversible
  uninstall that preserves user-modified entries.
- Managed `OSC 9;9` PowerShell profile integration so duplicated panes inherit the
  working directory, with encoding preservation and idempotent, reversible install.
- Per-monitor DPI awareness so hook coordinates and UI Automation rectangles share a
  coordinate system.
- `winter`, `winterminalp`, and hidden `winterd` binaries sharing one CLI.
- `plan`, `install`, `uninstall`, `doctor`, `config`, `run`, and `launch` commands.
- One-time `install.ps1` that builds the release binaries and puts `winter` on `PATH`.
- `winter` and `winter launch` now install the Windows Terminal integration
  automatically on first launch, so a fresh setup no longer needs a separate
  `winter install` step.
- UAC self-relaunch for controller entry points and elevation checks in the core.

### Fixed

- Accept Windows Terminal command-style keybindings that omit an `id` and reserve every
  chord of multi-chord entries instead of treating them as malformed.
- Replay the configured prefix directly so custom prefixes work without reinstalling the
  bridge.
- Pass pointer moves through during divider drags so the hardware cursor keeps reporting
  positions and the drag delta advances.
- Focus the leading pane once per drag instead of on every resize step.

[Unreleased]: https://github.com/raincfhnj/winterminalp/compare/v0.2.0...HEAD
[0.2.0]: https://github.com/raincfhnj/winterminalp/releases/tag/v0.2.0
