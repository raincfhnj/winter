# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Fixed

- `winter ui` no longer reports a running controller as offline while the
  workspace is idle. The controller wrote `dashboard.json` only when the
  observable state changed, but liveness is inferred from that file's age, so
  two quiet seconds were enough to cross the 2s staleness window and replace
  every live panel with `OFFLINE`. The observer now refreshes the file once a
  second even when nothing changed (`DASHBOARD_HEARTBEAT_MS`, guarded by a
  compile-time assertion that it stays inside the staleness window); writes
  still happen on change as before, and an idle session simply stays online.
- The dashboard says `1 pane`, not `1 panes`.
- A system switch key (`Alt+Tab`, `Ctrl+Tab`, `Alt+F4`, …) cancelling an armed
  prefix now ends the session *and* its suppressed-key ledger. Previously only
  the state was reset, so the consumed prefix key stayed suppressed: its key-up
  was swallowed after the session had already died, and the next physical
  prefix press was eaten as a stale release, forcing the user to press the
  prefix twice before it armed again.
- Mouse divider dragging no longer moves the wrong separator in nested layouts.
  Winter proved nothing about *which* separator `resizePane` would move; in a
  layout such as `[A|B]|C` the request recursed into the subtree containing the
  focused pane and resized the inner `A|B` separator the parser had inferred
  from the rectangles. Dragging now focuses a pane from which the separator
  under the pointer is provably the one Windows Terminal resizes, verified
  against every split tree consistent with the observed rectangles. Where that
  cannot be proven the drag is not captured and the click passes through to
  Windows Terminal instead of moving an unrelated separator.
- Legacy migration sweeps both historical shell-marker spellings. The abandoned
  project was branded `WinTerminalPP` and its prompt wrapper used that
  spelling, but the sweep only knew the single-`P` `WinTerminalP` markers, so an
  existing install kept a live `OSC 9;9` wrapper next to the new Winter block —
  double-wrapped prompts on every PowerShell session, while `doctor` reported a
  clean migration. The old test could not catch it: it asserted
  `!contains("WinTerminalP")`, and `WinTerminalPP` contains that substring.
- Loading a configuration file no longer writes it back. A schema 1 file was
  migrated on disk by `migrate_schema`, which meant the read-only commands
  (`winter config`, `winter doctor`) silently rewrote user configuration on
  every run. Legacy files are now migrated in memory only and keep their
  `schema_version = 1` until the user saves a new configuration; each load
  applies the same defaults, so behaviour is unchanged.
- `mouse_resize` tuning values are validated even when the feature is disabled.
  A disabled feature used to skip the range checks entirely, so an out-of-range
  `divider_hit_slop_px` or `geometry_poll_interval_ms` was accepted and would
  take effect as soon as `enabled` was flipped back on — without the file being
  re-validated.
- An action bound to the prefix chord is now rejected at load time. The prefix
  owns the first stage of every sequence and `handle_idle_key_down` matches it
  before consulting the action bindings, so `prefix = "ctrl+a"` together with
  `new_tab = "ctrl+a"` produced a silently unreachable action instead of a
  configuration error.
- Persistent autostart now refuses to register the logon task for an executable
  an unprivileged process of this account could replace. The task starts
  `winterd.exe` with `HighestAvailable`, and approving it once is the only
  approval it ever needs — but the program it runs needed none, so a task
  pointing into a writable directory (the default Cargo install location,
  `%USERPROFILE%\.cargo\bin`, is one) was a persistence primitive: replace the
  binary now, be elevated at the next sign-in. Before registering, every
  directory from the executable up to the volume root is inspected through
  `GetFileSecurityW`, and any grant of write, delete, or re-permission access to
  Everyone, Authenticated Users, `BUILTIN\Users`, an interactive/AppContainer
  SID, the running account's own SID, or a group its token holds enabled is a
  refusal. Unreadable descriptors, an absent DACL, and ACE types Winter cannot
  interpret are refusals too, never passes. `winter autostart enable` reports
  the offending directory, the trustee, and where to install instead; `winter
  autostart status` carries the verdict as `locationProtected` /
  `locationDirectory` / `locationIssue`; `winter install` reports `Skipped` with
  the same reason. There is deliberately no switch to bypass the check.
- `install.ps1` no longer installs the controller from the user-writable Cargo
  bin directory. It builds the release binaries, deploys `winter.exe`,
  `winterminalp.exe`, and `winterd.exe` to `%ProgramFiles%\Winter` (writable
  only by administrators), puts that directory on the machine `PATH` once, and
  runs `winter install` from the deployed copy so the task points at a protected
  executable. It requires administrator rights instead of silently degrading,
  keeps the previous binaries as a backup, restores them and reverts the `PATH`
  entry when `winter install` fails, and is idempotent on re-runs. The Cargo bin
  copy remains as the interactive entry point; it cannot be persisted, and the
  status report says so.
- The scheduled-task definition is no longer staged under a predictable name in
  the shared temporary directory. `%TEMP%\winter-task-<pid>.xml` was writable by
  every process of the account, the process id is guessable, and the write
  followed symbolic links, so the staging step could be redirected into an
  arbitrary file. The definition is now written with `create_new` — which fails
  on any existing entry, including a link — to an unpredictable name inside
  `%LOCALAPPDATA%\Winter\staging`, and the staging file is removed on every
  path, successful or not.
- `winter autostart disable` now reports the task that is actually on disk after
  the elevated helper exits. The non-elevated branch returned `Removed` from the
  snapshot taken *before* the UAC prompt, so a helper that failed, was dismissed
  late, or removed nothing still produced "removed"; a surviving task is now
  reported as still registered, a foreign task as foreign, and an unreadable
  task store as an incomplete operation instead of a success.

### Added

- `winter ui`: a tmux-style session manager for the terminal it runs in.
  The left sidebar shows live controller telemetry (uptime, prefix, hook
  health, action counters, last dispatch error) plus a `── sessions ──`
  tree of Windows Terminal tabs with their panes nested underneath; the
  right zone draws the pane map, and a tmux-style status bar runs along
  the bottom. `↑`/`↓` (or `k`/`j`) move the cursor and `Enter` focuses the
  selected pane or switches to the selected tab — requests travel to the
  controller through a one-shot `command.json`, executed on the observer
  tick and surfaced back through the same telemetry. Pane titles come from
  UI Automation, so renaming a tab labels its session. State is published
  to `%LOCALAPPDATA%\Winter\dashboard.json` on every observable change;
  `--once` renders a single frame for scripts.
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

- Pane splitting no longer drops back to the default directory for PowerShell 7.
  A missing `Documents\PowerShell` folder used to be reported as `Skipped` even
  when `pwsh.exe` was installed, so the shell that most users actually run never
  received the managed `OSC 9;9` prompt wrapper and every duplicate pane fell
  back to Windows Terminal's `%USERPROFILE%` default. `winter install` now probes
  for the host executable (`PATH`, including the Microsoft Store app execution
  alias, plus the `%ProgramFiles%\PowerShell\7[-preview]` roots) and creates the
  profile folder it needs; only a genuinely absent shell is still left untouched.
- The managed prompt wrapper now reports `OSC 9;9` only while the session is on
  the `FileSystem` provider (`$loc.ProviderPath`). A PSDrive such as `HKLM:\` was
  previously reported verbatim, and Windows Terminal 1.25 and earlier accepts any
  non-empty report, hands it to `CreateProcess`, and fails the split instead of
  falling back to the profile's directory.
- `winter install` also sweeps a leftover pre-Winter prompt wrapper. The one-shot
  migration only removes those blocks while legacy state still exists, so a
  machine whose legacy directory was already gone kept a live wrapper beside the
  managed one: every Windows PowerShell prompt reported `OSC 9;9` twice, and the
  old wrapper has no provider guard, so a session on a PSDrive such as `HKLM:\`
  made the next split fail on Windows Terminal 1.25 and earlier. The per-profile
  sweep result now keeps its informative status — a removal or a malformed marker
  pair is no longer masked by the `Missing` report of the second marker spelling,
  which also means the migration reports a broken legacy block instead of
  passing it silently.
- Fragment ownership records survive external reformatting: a semantically
  unchanged fragment no longer drops its manifest record, so `winter uninstall`
  can still remove it instead of reporting `Missing`.
- Unmanaged malformed `keybindings` entries are reported as warnings instead of
  blocking `install`/`uninstall` for every channel; literal `+` chords
  (`ctrl++`) parse as the `plus` key.
- Failed atomic writes no longer leave `.winter-tmp-*.json` staging files
  (or mismatched backups) in Windows Terminal's directories.
- A single unreadable or symlinked `settings.json` no longer aborts
  installation for all channels; symlinked settings files are not discovered.
- A missed key-up can no longer swallow the next keystroke in any application,
  and a stale shutdown handshake cannot fire minutes later on an unrelated
  keypress.
- `winterd` records panics in `last-error.log` (with a `%TEMP%` fallback) and
  exits instead of failing invisibly; `winter doctor` diagnoses invalid
  configurations instead of aborting before any report.
- UTF-16LE `settings.json` (Notepad's default save encoding) is now parsed
  and written back in its original encoding instead of failing with a UTF-8
  error; UTF-16BE and truncated files get clear, actionable messages.
- A left click now cancels an armed prefix even when `mouse_resize` is
  disabled: the mouse hook is always installed, with a zero-work fast path
  for move/release events while drag is off.
- Integration backups are pruned to the newest ten per label after each
  successful backup, so `state_dir/backups` no longer grows without bound.

### Changed

- The project is renamed to **Winter**: GitHub repo `raincfhnj/winter` (the old
  repository URL redirects there), package/crate `winter`, primary CLI
  `winter` unchanged; the compatibility alias `winterminalp` is kept. Every
  on-disk identifier is now Winter-branded (`%LOCALAPPDATA%\Winter`,
  `Fragments\Winter`, `User.Winter.*`, `# >>> Winter shell integration >>>`),
  and `winter install` auto-migrates historical `WinTerminalP` installations:
  it removes the legacy keybindings, fragment, and shell block using the old
  manifest's own records, carries `config.toml` over, and deletes the legacy
  state directory — `winter uninstall` also cleans a legacy-only install, and
  `plan`/`doctor` surface a pending migration as an issue.
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

[Unreleased]: https://github.com/raincfhnj/winter/compare/v0.2.0...HEAD
[0.2.0]: https://github.com/raincfhnj/winter/releases/tag/v0.2.0
